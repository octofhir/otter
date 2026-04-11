//! Bytecode dispatch: the single `match` over every `Opcode` variant (kept
//! whole to preserve LLVM's jump-table lowering), plus private `Interpreter`
//! helpers called from `step`: operand decoding, call-argument marshalling,
//! host-function invocation, construct-receiver allocation, generator /
//! async-generator resume kernels.

use crate::bytecode::{BytecodeRegister, Opcode, ProgramCounter};
use crate::call::{ClosureCall, DirectCall};
use crate::closure::{CaptureDescriptor, ClosureTemplate, UpvalueId};
use crate::descriptors::{NativeFunctionDescriptor, NativeSlotKind, VmNativeCallError};
use crate::float::FloatId;
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::host::HostFunctionId;
use crate::module::{Function, Module};
use crate::object::{HeapValueKind, ObjectError, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::property::PropertyNameId;
use crate::string::StringId;
use crate::value::RegisterValue;

use super::step_outcome::{Completion, StepOutcome, TailCallPayload, YieldStarResult};
use super::{
    Activation, FrameRuntimeState, Interpreter, InterpreterError, RuntimeState, ToPrimitiveHint,
    EXECUTION_INTERRUPTED_MESSAGE,
};

impl Interpreter {
    pub(super) fn step(
        &self,
        function: &Function,
        module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
    ) -> Result<StepOutcome, InterpreterError> {
        let instruction = activation
            .instruction(function)
            .ok_or(InterpreterError::UnexpectedEndOfBytecode)?;

        match instruction.opcode() {
            Opcode::Nop => {
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Move => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadI32 => {
                let value = RegisterValue::from_i32(instruction.immediate_i32());
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadTrue => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(true),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadFalse => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(false),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadNaN => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(f64::NAN),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadF64 => {
                let float_id = FloatId(instruction.b());
                let value = function
                    .float_constants()
                    .get(float_id)
                    .ok_or(InterpreterError::InvalidConstant)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(value),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §6.1.6.2 Load BigInt constant from side table.
            // <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
            Opcode::LoadBigInt => {
                let bigint_id = crate::bigint::BigIntId(instruction.b());
                let value_str = function
                    .bigint_constants()
                    .get(bigint_id)
                    .ok_or(InterpreterError::InvalidConstant)?;
                let handle = runtime.alloc_bigint(value_str);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bigint_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewObject => {
                let handle = runtime.alloc_object();
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadString => {
                let string = Self::resolve_string_literal(function, instruction.b())?;
                let handle = runtime.alloc_js_string(string);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §22.2.3 — RegExpLiteral evaluation: allocate a fresh RegExp object.
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-regularexpressionliteral>
            Opcode::NewRegExp => {
                let entry = Self::resolve_regexp_literal(function, instruction.b())?;
                let prototype = runtime.intrinsics().regexp_prototype();
                // §22.2.3.1 RegExpCreate — the runtime-level helper handles
                // the spec-mandated `lastIndex` own-property initialization.
                let pattern = entry.pattern.to_string();
                let flags = entry.flags.to_string();
                let handle = runtime.alloc_regexp(&pattern, &flags, Some(prototype));
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewArray => {
                let handle = runtime.alloc_array();
                let len = instruction.b() as usize;
                if len > 0 {
                    runtime.objects_mut().set_array_length(handle, len)?;
                }
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewClosure => {
                let template = Self::resolve_closure_template(function, activation.pc())?;
                let mut upvalues = Vec::with_capacity(usize::from(template.capture_count()));

                for capture in template.captures() {
                    let upvalue = match capture {
                        CaptureDescriptor::Register(register) => activation
                            .capture_bytecode_register_upvalue(function, runtime, *register)?,
                        CaptureDescriptor::Upvalue(upvalue) => {
                            Self::resolve_upvalue_cell(activation, runtime, *upvalue)?
                        }
                    };
                    upvalues.push(upvalue);
                }

                let flags = template.flags();
                let handle = runtime.alloc_closure(template.callee(), upvalues, flags);
                // §15.3 ArrowFunction lexical inheritance — arrows capture
                // the enclosing function's closure so `super` / `new.target`
                // resolve through the lexical chain rather than the current
                // activation at call time.
                if flags.is_arrow() {
                    let parent = activation.closure_handle();
                    let active_new_target = activation.construct_new_target();
                    runtime
                        .objects
                        .set_arrow_lexical_context(handle, parent, active_new_target)?;
                }
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // -----------------------------------------------------------------
            // ES2024 §10.4.4 CreateArguments — creates the arguments exotic object.
            //
            // Collects formal parameter values from the activation register file
            // and overflow arguments from `activation.overflow_args`, then builds
            // an arguments object with:
            //   - Indexed element access (§10.4.4.1 [[GetOwnProperty]])
            //   - `length` property = actual argument count (§10.4.4.6 step 7)
            //   - `callee` property = current closure (sloppy mode, §10.4.4.7 step 13)
            //   - Prototype = %Object.prototype% (NOT Array.prototype)
            // -----------------------------------------------------------------
            Opcode::CreateArguments => {
                let actual_argc = activation.metadata.argument_count();
                let param_count = function.frame_layout().parameter_count();

                // Collect all actual arguments: formal params from registers + overflow.
                // Parameter slots are user-visible registers 0..param_count.
                let mut all_args = Vec::with_capacity(usize::from(actual_argc));
                let copy_from_regs = actual_argc.min(param_count);
                for i in 0..copy_from_regs {
                    let value = activation.read_bytecode_register(function, i)?;
                    all_args.push(value);
                }
                for overflow_val in &activation.overflow_args {
                    all_args.push(*overflow_val);
                }

                // §10.4.4 — The arguments object is an ordinary object (NOT
                // an Array exotic). Create as a regular object with
                // %Object.prototype% and install indexed elements + length.
                let obj_proto = runtime.intrinsics().object_prototype();
                let args_obj = runtime.alloc_object_with_prototype(Some(obj_proto));
                for (index, &value) in all_args.iter().enumerate() {
                    let key = runtime.intern_property_name(&index.to_string());
                    runtime
                        .objects_mut()
                        .define_own_property(
                            args_obj,
                            key,
                            PropertyValue::data_with_attrs(value, PropertyAttributes::data()),
                        )
                        .ok();
                }

                // §10.4.4.6 step 7: Install `length` as own data property {W:true, E:false, C:true}.
                let length_key = runtime.intern_property_name("length");
                runtime
                    .objects_mut()
                    .define_own_property(
                        args_obj,
                        length_key,
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_i32(i32::from(actual_argc)),
                            PropertyAttributes::builtin_method(),
                        ),
                    )
                    .ok();

                // §10.4.4.6 step 13 / §10.4.4.7 step 8: Install `callee`.
                let callee_key = runtime.intern_property_name("callee");
                if function.is_strict() {
                    // §10.4.4.7 step 8: Unmapped arguments — accessor with %ThrowTypeError%.
                    // { [[Get]]: %ThrowTypeError%, [[Set]]: %ThrowTypeError%,
                    //   [[Enumerable]]: false, [[Configurable]]: false }
                    let thrower = runtime
                        .intrinsics()
                        .throw_type_error_function()
                        .expect("%ThrowTypeError% intrinsic must be initialised by this point");
                    runtime
                        .objects_mut()
                        .define_own_property(
                            args_obj,
                            callee_key,
                            PropertyValue::Accessor {
                                getter: Some(thrower),
                                setter: Some(thrower),
                                attributes: PropertyAttributes::constant(),
                            },
                        )
                        .expect("strict-mode callee accessor should install");
                } else if let Some(closure) = activation.closure_handle() {
                    // §10.4.4.6 step 13: Mapped arguments — data property with callee.
                    // { [[Value]]: func, [[Writable]]: true,
                    //   [[Enumerable]]: false, [[Configurable]]: true }
                    runtime
                        .objects_mut()
                        .define_own_property(
                            args_obj,
                            callee_key,
                            PropertyValue::data_with_attrs(
                                RegisterValue::from_object_handle(closure.0),
                                PropertyAttributes::builtin_method(),
                            ),
                        )
                        .ok();
                }

                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(args_obj.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CreateRestParameters => {
                let rest_array = runtime.alloc_array_with_elements(&activation.overflow_args);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(rest_array.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CreateEnumerableOwnKeys => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let keys =
                    runtime
                        .enumerable_own_property_keys(handle)
                        .map_err(|error| match error {
                            VmNativeCallError::Thrown(_) => {
                                InterpreterError::TypeError("enumerable own keys threw".into())
                            }
                            VmNativeCallError::Internal(message) => {
                                InterpreterError::NativeCall(message)
                            }
                        })?;
                let key_names = keys
                    .into_iter()
                    .filter_map(|key| runtime.property_names().get(key))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let key_values = key_names
                    .into_iter()
                    .map(|name| RegisterValue::from_object_handle(runtime.alloc_string(name).0))
                    .collect::<Vec<_>>();
                let keys_array = runtime.alloc_array_with_elements(&key_values);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(keys_array.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadHole => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::hole(),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::AssertNotHole => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                if value.is_hole() {
                    let error =
                        runtime.alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::DefineNamedGetter
            | Opcode::DefineNamedSetter
            | Opcode::DefineComputedGetter
            | Opcode::DefineComputedSetter => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(object)?;
                let (property, accessor_register) = match instruction.opcode() {
                    Opcode::DefineNamedGetter | Opcode::DefineNamedSetter => (
                        Self::resolve_property_name(function, runtime, instruction.c())?,
                        instruction.b(),
                    ),
                    Opcode::DefineComputedGetter | Opcode::DefineComputedSetter => {
                        let key = activation.read_bytecode_register(function, instruction.b())?;
                        (runtime.computed_property_name(key)?, instruction.c())
                    }
                    _ => unreachable!(),
                };
                let accessor = activation
                    .read_bytecode_register(function, accessor_register)?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let desc = match instruction.opcode() {
                    Opcode::DefineNamedGetter => crate::object::PropertyDescriptor::accessor(
                        Some(Some(accessor)),
                        None,
                        Some(true),
                        Some(true),
                    ),
                    Opcode::DefineNamedSetter => crate::object::PropertyDescriptor::accessor(
                        None,
                        Some(Some(accessor)),
                        Some(true),
                        Some(true),
                    ),
                    Opcode::DefineComputedGetter => crate::object::PropertyDescriptor::accessor(
                        Some(Some(accessor)),
                        None,
                        Some(true),
                        Some(true),
                    ),
                    Opcode::DefineComputedSetter => crate::object::PropertyDescriptor::accessor(
                        None,
                        Some(Some(accessor)),
                        Some(true),
                        Some(true),
                    ),
                    _ => unreachable!(),
                };
                let defined = runtime
                    .objects
                    .define_own_property_from_descriptor(handle, property, desc)?;
                if !defined {
                    return Err(InterpreterError::TypeError(
                        "object literal accessor define failed".into(),
                    ));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.4.5 MethodDefinitionEvaluation for class methods — install a
            // data method on the class prototype/constructor with
            // [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: true.
            // Unlike SetProperty this uses [[DefineOwnProperty]] and bypasses
            // any inherited setters.
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-methoddefinitionevaluation>
            Opcode::DefineClassMethod | Opcode::DefineClassMethodComputed => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(object)?;
                // For computed keys we also need the raw key value so we can
                // run §10.2.9 SetFunctionName on the method closure before it
                // is installed on the class prototype/constructor.
                let mut computed_key: Option<RegisterValue> = None;
                let (property, method_value) = match instruction.opcode() {
                    Opcode::DefineClassMethod => {
                        let method =
                            activation.read_bytecode_register(function, instruction.b())?;
                        (
                            Self::resolve_property_name(function, runtime, instruction.c())?,
                            method,
                        )
                    }
                    Opcode::DefineClassMethodComputed => {
                        let key = activation.read_bytecode_register(function, instruction.b())?;
                        computed_key = Some(key);
                        let method =
                            activation.read_bytecode_register(function, instruction.c())?;
                        (runtime.computed_property_name(key)?, method)
                    }
                    _ => unreachable!(),
                };
                // §15.4.5 MethodDefinitionEvaluation step 7 — SetFunctionName
                // for methods created with computed keys so that e.g.
                // `class A { [Symbol('s')]() {} }` gives the method
                // `name === "[s]"`.
                if let Some(key) = computed_key
                    && let Some(closure_handle) = method_value.as_object_handle().map(ObjectHandle)
                {
                    runtime.update_closure_function_name(closure_handle, key, None)?;
                }
                let desc = crate::object::PropertyDescriptor::data(
                    Some(method_value),
                    Some(true),  // writable
                    Some(false), // enumerable — §15.7.14 step 28
                    Some(true),  // configurable
                );
                let defined = runtime
                    .objects
                    .define_own_property_from_descriptor(handle, property, desc)?;
                if !defined {
                    return Err(InterpreterError::TypeError(
                        "class method define failed".into(),
                    ));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 — Install a non-enumerable accessor (getter/setter) on a
            // class prototype/constructor. Mirrors DefineNamedGetter/Setter but
            // forces [[Enumerable]]: false.
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
            Opcode::DefineClassGetter
            | Opcode::DefineClassSetter
            | Opcode::DefineClassGetterComputed
            | Opcode::DefineClassSetterComputed => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(object)?;
                let mut computed_key: Option<RegisterValue> = None;
                let (property, accessor_register) = match instruction.opcode() {
                    Opcode::DefineClassGetter | Opcode::DefineClassSetter => (
                        Self::resolve_property_name(function, runtime, instruction.c())?,
                        instruction.b(),
                    ),
                    Opcode::DefineClassGetterComputed | Opcode::DefineClassSetterComputed => {
                        let key = activation.read_bytecode_register(function, instruction.b())?;
                        computed_key = Some(key);
                        (runtime.computed_property_name(key)?, instruction.c())
                    }
                    _ => unreachable!(),
                };
                let accessor = activation
                    .read_bytecode_register(function, accessor_register)?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                // §15.4.5 MethodDefinitionEvaluation — SetFunctionName for
                // computed-key getters/setters with the "get"/"set" prefix.
                if let Some(key) = computed_key {
                    let prefix = match instruction.opcode() {
                        Opcode::DefineClassGetterComputed => Some("get"),
                        Opcode::DefineClassSetterComputed => Some("set"),
                        _ => None,
                    };
                    runtime.update_closure_function_name(accessor, key, prefix)?;
                }
                let desc = match instruction.opcode() {
                    Opcode::DefineClassGetter | Opcode::DefineClassGetterComputed => {
                        crate::object::PropertyDescriptor::accessor(
                            Some(Some(accessor)),
                            None,
                            Some(false), // enumerable
                            Some(true),  // configurable
                        )
                    }
                    Opcode::DefineClassSetter | Opcode::DefineClassSetterComputed => {
                        crate::object::PropertyDescriptor::accessor(
                            None,
                            Some(Some(accessor)),
                            Some(false),
                            Some(true),
                        )
                    }
                    _ => unreachable!(),
                };
                let defined = runtime
                    .objects
                    .define_own_property_from_descriptor(handle, property, desc)?;
                if !defined {
                    return Err(InterpreterError::TypeError(
                        "class accessor define failed".into(),
                    ));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §10.2.5 MakeMethod — install [[HomeObject]] on a method closure.
            // The compiler emits this instruction for every method, getter,
            // setter, object-literal short-method, so that subsequent
            // `super.foo` / `super[x]` inside the method body resolves to
            // `HomeObject.[[Prototype]]`.
            // Spec: <https://tc39.es/ecma262/#sec-makemethod>
            // §13.3.12 MetaProperty `new.target`.
            // For construct calls: returns the active new-target.
            // For arrows: returns the lexically captured new-target from
            //   the enclosing construct context.
            // For all other calls: returns undefined.
            // Spec: <https://tc39.es/ecma262/#sec-meta-properties-runtime-semantics-evaluation>
            Opcode::LoadNewTarget => {
                let value = if activation.metadata().flags().is_construct() {
                    activation
                        .construct_new_target()
                        .or(activation.closure_handle())
                        .map(|h| RegisterValue::from_object_handle(h.0))
                        .unwrap_or_default()
                } else if let Some(closure) = activation.closure_handle() {
                    runtime
                        .objects
                        .closure_captured_new_target(closure)?
                        .map(|h| RegisterValue::from_object_handle(h.0))
                        .unwrap_or(RegisterValue::undefined())
                } else {
                    RegisterValue::undefined()
                };
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.15 — Runtime TypeError for assignment to immutable
            // class name binding (`class C { m() { C = 42; } }`).
            Opcode::ThrowConstAssign => {
                let error = runtime.alloc_type_error("Assignment to constant variable.")?;
                Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                    error.0,
                )))
            }
            // §15.7.14 step 5.f — Assert superclass is a constructor.
            Opcode::AssertConstructor => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                let is_ctor = value
                    .as_object_handle()
                    .map(ObjectHandle)
                    .is_some_and(|h| runtime.is_constructible(h));
                if !is_ctor {
                    let error = runtime
                        .alloc_type_error("Class extends value is not a constructor or null")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §7.1.14 ToPropertyKey — convert value to String or Symbol.
            // Used to pre-evaluate computed class field keys at class definition
            // time so that ToPrimitive/ToString errors fire before any instance
            // is created.
            // Spec: <https://tc39.es/ecma262/#sec-topropertykey>
            Opcode::ToPropertyKey => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                // ToPrimitive(value, string)
                let primitive =
                    runtime.js_to_primitive_with_hint(value, ToPrimitiveHint::String)?;
                // If Symbol, keep as-is; otherwise ToString.
                let result = if primitive.as_symbol_id().is_some() {
                    primitive
                } else {
                    let s = runtime.js_to_string(primitive)?;
                    let handle = runtime.alloc_string(s);
                    RegisterValue::from_object_handle(handle.0)
                };
                activation.write_bytecode_register(function, instruction.a(), result)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetHomeObject => {
                let closure_value = activation.read_bytecode_register(function, instruction.a())?;
                let home_value = activation.read_bytecode_register(function, instruction.b())?;
                let closure_handle = closure_value
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let home_handle = home_value
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                runtime
                    .objects
                    .set_closure_home_object(closure_handle, home_handle)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.3.7 GetValue on a super reference — `super.foo` / `super[x]`.
            // The base object is `HomeObject.[[Prototype]]`; the receiver used
            // for accessor calls is the current `this` value.
            // Spec: <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>
            Opcode::GetSuperProperty | Opcode::GetSuperPropertyComputed => {
                let property = match instruction.opcode() {
                    Opcode::GetSuperProperty => {
                        Self::resolve_property_name(function, runtime, instruction.b())?
                    }
                    Opcode::GetSuperPropertyComputed => {
                        let key = activation.read_bytecode_register(function, instruction.b())?;
                        runtime.computed_property_name(key)?
                    }
                    _ => unreachable!(),
                };
                let closure_handle = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let home = runtime
                    .objects
                    .closure_home_object(closure_handle)?
                    .ok_or_else(|| {
                        InterpreterError::TypeError("super property access outside a method".into())
                    })?;
                let base = match runtime.objects.get_prototype(home)? {
                    Some(proto) => proto,
                    None => {
                        return Err(InterpreterError::TypeError(
                            "cannot read super property on an object with null prototype".into(),
                        ));
                    }
                };
                // Receiver for accessor calls is the current `this`.
                let receiver = activation.receiver(function)?;
                let value = match runtime.property_lookup(base, property)? {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Data { value, .. } => value,
                        PropertyValue::Accessor { getter, .. } => {
                            runtime.call_callable_for_accessor(getter, receiver, &[])?
                        }
                    },
                    None => RegisterValue::undefined(),
                };
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.3.7 PutValue on a super reference — `super.foo = value` /
            // `super[x] = value`. The base object is `HomeObject.[[Prototype]]`;
            // the receiver used for the write is the current `this`.
            Opcode::SetSuperProperty | Opcode::SetSuperPropertyComputed => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                let property = match instruction.opcode() {
                    Opcode::SetSuperProperty => {
                        Self::resolve_property_name(function, runtime, instruction.b())?
                    }
                    Opcode::SetSuperPropertyComputed => {
                        let key = activation.read_bytecode_register(function, instruction.b())?;
                        runtime.computed_property_name(key)?
                    }
                    _ => unreachable!(),
                };
                let closure_handle = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let home = runtime
                    .objects
                    .closure_home_object(closure_handle)?
                    .ok_or_else(|| {
                        InterpreterError::TypeError(
                            "super property assignment outside a method".into(),
                        )
                    })?;
                let base = match runtime.objects.get_prototype(home)? {
                    Some(proto) => proto,
                    None => {
                        return Err(InterpreterError::TypeError(
                            "cannot write super property on an object with null prototype".into(),
                        ));
                    }
                };
                let receiver = activation.receiver(function)?;
                // If the resolved slot is an accessor, call its setter with
                // `this` as the receiver. Otherwise, perform a regular
                // [[Set]] on `this` (not on the base) — this is how
                // OrdinaryObject [[Set]] with receiver=this behaves for
                // classes.
                match runtime.property_lookup(base, property)? {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Accessor { setter, .. } => {
                            runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                        }
                        PropertyValue::Data { .. } => {
                            let receiver_handle = runtime.property_base_object_handle(receiver)?;
                            runtime
                                .objects
                                .set_property(receiver_handle, property, value)?;
                        }
                    },
                    None => {
                        let receiver_handle = runtime.property_base_object_handle(receiver)?;
                        runtime
                            .objects
                            .set_property(receiver_handle, property, value)?;
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 DefineField — define own data property with named key.
            // Spec: <https://tc39.es/ecma262/#sec-definefield>
            Opcode::DefineField => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(object)?;
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let desc = crate::object::PropertyDescriptor::data(
                    Some(value),
                    Some(true), // writable
                    Some(true), // enumerable
                    Some(true), // configurable
                );
                let defined = runtime
                    .objects
                    .define_own_property_from_descriptor(handle, property, desc)?;
                if !defined {
                    return Err(InterpreterError::TypeError(
                        "class field define failed".into(),
                    ));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 DefineField — computed key variant.
            // Spec: <https://tc39.es/ecma262/#sec-definefield>
            Opcode::DefineComputedField => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(object)?;
                let key = activation.read_bytecode_register(function, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.c())?;
                let property = runtime.computed_property_name(key)?;
                let desc = crate::object::PropertyDescriptor::data(
                    Some(value),
                    Some(true), // writable
                    Some(true), // enumerable
                    Some(true), // configurable
                );
                let defined = runtime
                    .objects
                    .define_own_property_from_descriptor(handle, property, desc)?;
                if !defined {
                    return Err(InterpreterError::TypeError(
                        "class computed field define failed".into(),
                    ));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 RunClassFieldInitializer — InitializeInstanceElements.
            // Step 1: copy [[PrivateMethods]] from constructor to instance.
            // Step 2: invoke the field initializer with `this` as receiver.
            // Spec: <https://tc39.es/ecma262/#sec-initializeinstanceelements>
            Opcode::RunClassFieldInitializer => {
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;

                // Step 1: Copy private methods from constructor to instance.
                let private_methods = runtime.objects.closure_private_methods(closure)?;
                if !private_methods.is_empty() {
                    let this_value = activation.receiver(function)?;
                    let this_handle = this_value
                        .as_object_handle()
                        .map(ObjectHandle)
                        .ok_or(InterpreterError::InvalidObjectValue)?;
                    for (key, element) in private_methods {
                        runtime.objects.private_method_or_accessor_add(
                            this_handle,
                            key,
                            element,
                        )?;
                    }
                }

                // Step 2: Run field initializer (handles both public and private fields).
                let initializer = runtime.objects.closure_field_initializer(closure)?;
                if let Some(init_handle) = initializer {
                    let this_value = activation.receiver(function)?;
                    // §B.3.5.2 — Track field initializer depth for eval restrictions.
                    runtime.field_initializer_depth += 1;
                    let result = Self::call_function(runtime, module, init_handle, this_value, &[]);
                    runtime.field_initializer_depth -= 1;
                    match result {
                        Ok(_) => {
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Err(InterpreterError::UncaughtThrow(value)) => {
                            Ok(StepOutcome::Throw(value))
                        }
                        Err(other) => Err(other),
                    }
                } else {
                    activation.advance();
                    Ok(StepOutcome::Continue)
                }
            }
            // §15.7.14 SetClassFieldInitializer — store initializer on constructor.
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
            Opcode::SetClassFieldInitializer => {
                let constructor = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let initializer = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                runtime
                    .objects
                    .set_closure_field_initializer(constructor, initializer)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // ── Private Class Elements ─────────────────────────────────────

            // §6.2.12 AllocClassId — allocate unique class_id on a closure.
            // Spec: <https://tc39.es/ecma262/#sec-private-names>
            Opcode::AllocClassId => {
                let closure = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let id = runtime.alloc_class_id();
                runtime.objects.set_closure_class_id(closure, id)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §6.2.12 CopyClassId — copy class_id between closures.
            // Spec: <https://tc39.es/ecma262/#sec-private-names>
            Opcode::CopyClassId => {
                let target = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let source = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let id = runtime.objects.closure_class_id(source)?;
                runtime.objects.set_closure_class_id(target, id)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §7.3.31 DefinePrivateField — PrivateFieldAdd.
            // Spec: <https://tc39.es/ecma262/#sec-privatefieldadd>
            Opcode::DefinePrivateField => {
                let obj_handle = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let class_id = Self::resolve_class_id(activation, runtime, Some(obj_handle))?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.private_field_add(obj_handle, key, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §7.3.32 GetPrivateField — PrivateGet.
            // Spec: <https://tc39.es/ecma262/#sec-privateget>
            Opcode::GetPrivateField => {
                let object = activation.read_bytecode_register(function, instruction.b())?;
                let obj_handle = object
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let class_id = runtime.objects.closure_class_id(closure)?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                // Check element kind to handle accessor getters.
                let element = runtime.objects.private_elements_ref(obj_handle, &key);
                match element {
                    Some(crate::object::PrivateElement::Accessor {
                        getter: Some(getter_handle),
                        ..
                    }) => {
                        let getter_handle = *getter_handle;
                        match Self::call_function(runtime, module, getter_handle, object, &[]) {
                            Ok(result) => {
                                activation.write_bytecode_register(
                                    function,
                                    instruction.a(),
                                    result,
                                )?;
                            }
                            Err(InterpreterError::UncaughtThrow(value)) => {
                                return Ok(StepOutcome::Throw(value));
                            }
                            Err(other) => return Err(other),
                        }
                    }
                    Some(crate::object::PrivateElement::Accessor { getter: None, .. }) => {
                        return Err(InterpreterError::TypeError(
                            "private accessor has no getter".into(),
                        ));
                    }
                    _ => {
                        let result = runtime.objects.private_get(obj_handle, &key)?;
                        activation.write_bytecode_register(function, instruction.a(), result)?;
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §7.3.33 SetPrivateField — PrivateSet.
            // Spec: <https://tc39.es/ecma262/#sec-privateset>
            Opcode::SetPrivateField => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let obj_handle = object
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let class_id = runtime.objects.closure_class_id(closure)?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                match runtime.objects.private_set(obj_handle, &key, value)? {
                    None => {} // Field set succeeded directly.
                    Some(setter_handle) => {
                        match Self::call_function(runtime, module, setter_handle, object, &[value])
                        {
                            Ok(_) => {}
                            Err(InterpreterError::UncaughtThrow(v)) => {
                                return Ok(StepOutcome::Throw(v));
                            }
                            Err(other) => return Err(other),
                        }
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 DefinePrivateMethod — static private method on object.
            // Spec: <https://tc39.es/ecma262/#sec-privatemethodoraccessoradd>
            Opcode::DefinePrivateMethod => {
                let object = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let method = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let class_id = Self::resolve_class_id(activation, runtime, Some(object))?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.private_method_or_accessor_add(
                    object,
                    key,
                    crate::object::PrivateElement::Method(method),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 DefinePrivateGetter — static private getter on object.
            Opcode::DefinePrivateGetter => {
                let object = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let getter = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let class_id = Self::resolve_class_id(activation, runtime, Some(object))?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.private_method_or_accessor_add(
                    object,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: Some(getter),
                        setter: None,
                    },
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 DefinePrivateSetter — static private setter on object.
            Opcode::DefinePrivateSetter => {
                let object = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let setter = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let class_id = Self::resolve_class_id(activation, runtime, Some(object))?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.private_method_or_accessor_add(
                    object,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: None,
                        setter: Some(setter),
                    },
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 PushPrivateMethod — instance private method on constructor.
            Opcode::PushPrivateMethod => {
                let constructor = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let method = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let class_id = runtime.objects.closure_class_id(constructor)?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.push_private_method(
                    constructor,
                    key,
                    crate::object::PrivateElement::Method(method),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 PushPrivateGetter — instance private getter on constructor.
            Opcode::PushPrivateGetter => {
                let constructor = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let getter = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let class_id = runtime.objects.closure_class_id(constructor)?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.push_private_method(
                    constructor,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: Some(getter),
                        setter: None,
                    },
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §15.7.14 PushPrivateSetter — instance private setter on constructor.
            Opcode::PushPrivateSetter => {
                let constructor = activation
                    .read_bytecode_register(function, instruction.a())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let setter = activation
                    .read_bytecode_register(function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let class_id = runtime.objects.closure_class_id(constructor)?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                runtime.objects.push_private_method(
                    constructor,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: None,
                        setter: Some(setter),
                    },
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.10.1 InPrivate — `#field in obj` brand check.
            Opcode::InPrivate => {
                let object = activation.read_bytecode_register(function, instruction.b())?;
                let obj_handle = object.as_object_handle().map(ObjectHandle).ok_or_else(|| {
                    InterpreterError::TypeError(
                        "right-hand side of 'in' should be an object".into(),
                    )
                })?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let class_id = runtime.objects.closure_class_id(closure)?;
                let key =
                    Self::resolve_private_name_key(function, runtime, instruction.c(), class_id)?;
                let found = runtime.objects.private_element_find(obj_handle, &key)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(found),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CopyDataProperties | Opcode::CopyDataPropertiesExcept => {
                let target = activation.read_bytecode_register(function, instruction.a())?;
                let target_handle = runtime.property_base_object_handle(target)?;
                let source = activation.read_bytecode_register(function, instruction.b())?;
                let excluded_keys = if instruction.opcode() == Opcode::CopyDataPropertiesExcept {
                    Some(activation.read_bytecode_register(function, instruction.c())?)
                } else {
                    None
                };
                match crate::property_copy::copy_data_properties(
                    runtime,
                    target_handle,
                    source,
                    excluded_keys,
                ) {
                    Ok(()) => {
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
            Opcode::LoadUndefined => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::undefined(),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadNull => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::null(),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadException => {
                let value = activation
                    .take_pending_exception()
                    .ok_or(InterpreterError::MissingPendingException)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadCurrentClosure => {
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(closure.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadThis => {
                let receiver = activation.receiver(function)?;
                if function.is_derived_constructor()
                    && activation.metadata().flags().is_construct()
                    && receiver == RegisterValue::undefined()
                {
                    let error = runtime.alloc_reference_error(
                        "Must call super constructor in derived class before accessing 'this'",
                    )?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
                activation.write_bytecode_register(function, instruction.a(), receiver)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::TypeOf => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let type_of = runtime.js_typeof(value)?;
                activation.write_bytecode_register(function, instruction.a(), type_of)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Not => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let truthy = runtime.js_to_boolean(value)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(!truthy),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::ToNumber => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let number = runtime.js_to_number(value)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(number),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::ToString => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let text = runtime.js_to_string(value)?;
                let string = runtime.alloc_string(text);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(string.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Add => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = runtime.js_add(lhs, rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Sub => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // §6.1.6.2.8 BigInt::subtract
                if lhs.is_bigint() && rhs.is_bigint() {
                    let result = runtime.bigint_binary_op(lhs, rhs, |a, b| a - b)?;
                    activation.write_bytecode_register(function, instruction.a(), result)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }
                if lhs.is_bigint() || rhs.is_bigint() {
                    return Err(InterpreterError::TypeError(
                        "Cannot mix BigInt and other types, use explicit conversions".into(),
                    ));
                }
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num - rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Mul => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // §6.1.6.2.9 BigInt::multiply
                if lhs.is_bigint() && rhs.is_bigint() {
                    let result = runtime.bigint_binary_op(lhs, rhs, |a, b| a * b)?;
                    activation.write_bytecode_register(function, instruction.a(), result)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }
                if lhs.is_bigint() || rhs.is_bigint() {
                    return Err(InterpreterError::TypeError(
                        "Cannot mix BigInt and other types, use explicit conversions".into(),
                    ));
                }
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num * rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Div => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // §6.1.6.2.10 BigInt::divide — throws RangeError for division by zero.
                if lhs.is_bigint() && rhs.is_bigint() {
                    let result = runtime.bigint_checked_div(lhs, rhs)?;
                    activation.write_bytecode_register(function, instruction.a(), result)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }
                if lhs.is_bigint() || rhs.is_bigint() {
                    return Err(InterpreterError::TypeError(
                        "Cannot mix BigInt and other types, use explicit conversions".into(),
                    ));
                }
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num / rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Eq => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(runtime.objects.strict_eq(lhs, rhs)?),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LooseEq => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(runtime.js_loose_eq(lhs, rhs)?),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // ES spec 7.2.13 Abstract Relational Comparison.
            // Lt(a, b, c): a = (b < c)
            Opcode::Lt => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // AbstractRelationalComparison(x, y, LeftFirst=true) → true means x < y
                let result = runtime
                    .js_abstract_relational_comparison(lhs, rhs, true)?
                    .unwrap_or(false);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Gt(a, b, c): a = (b > c) ≡ AbstractRelationalComparison(c, b, LeftFirst=false)
            Opcode::Gt => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let result = runtime
                    .js_abstract_relational_comparison(rhs, lhs, false)?
                    .unwrap_or(false);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Gte(a, b, c): a = (b >= c) ≡ !(c < b ... wait, no)
            // ES spec: x >= y ≡ NOT AbstractRelationalComparison(x, y) where undefined → false
            Opcode::Gte => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // x >= y: if AbstractRelationalComparison(x, y) is undefined or true → false
                let less = runtime.js_abstract_relational_comparison(lhs, rhs, true)?;
                let result = match less {
                    None => false,       // undefined (NaN) → false
                    Some(true) => false, // x < y → not >=
                    Some(false) => true, // x >= y
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Lte(a, b, c): a = (b <= c) ≡ NOT AbstractRelationalComparison(c, b, LeftFirst=false)
            Opcode::Lte => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let greater = runtime.js_abstract_relational_comparison(rhs, lhs, false)?;
                let result = match greater {
                    None => false,
                    Some(true) => false,
                    Some(false) => true,
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §6.1.6.2.11 BigInt::remainder / Mod uses ToNumber coercion.
            Opcode::Mod => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                if lhs.is_bigint() && rhs.is_bigint() {
                    let result = runtime.bigint_checked_rem(lhs, rhs)?;
                    activation.write_bytecode_register(function, instruction.a(), result)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }
                if lhs.is_bigint() || rhs.is_bigint() {
                    return Err(InterpreterError::TypeError(
                        "Cannot mix BigInt and other types, use explicit conversions".into(),
                    ));
                }
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num % rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §6.1.6.1.3 Number::exponentiate
            // Spec: <https://tc39.es/ecma262/#sec-exp-operator>
            Opcode::Exp => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                if lhs.is_bigint() || rhs.is_bigint() {
                    return Err(InterpreterError::TypeError(
                        "BigInt exponentiation not yet supported".into(),
                    ));
                }
                let base = runtime.js_to_number(lhs)?;
                let exponent = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(base.powf(exponent)),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::BitAnd => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_i32 = runtime.js_to_int32(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 & rhs_i32) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::BitOr => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_i32 = runtime.js_to_int32(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 | rhs_i32) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::BitXor => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_i32 = runtime.js_to_int32(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 ^ rhs_i32) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Shl => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_u32 = runtime.js_to_uint32(rhs)?;
                let shift = rhs_u32 & 0x1F;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 << shift) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Shr => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_u32 = runtime.js_to_uint32(rhs)?;
                let shift = rhs_u32 & 0x1F;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 >> shift) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::UShr => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_u32 = runtime.js_to_uint32(lhs)?;
                let rhs_u32 = runtime.js_to_uint32(rhs)?;
                let shift = rhs_u32 & 0x1F;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_u32 >> shift) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetProperty => {
                let pc = activation.pc();
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;

                // §10.5.8 — Proxy [[Get]] trap
                if runtime.is_proxy(handle) {
                    let value = runtime.proxy_get(handle, property, base)?;
                    activation.write_bytecode_register(function, instruction.a(), value)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let property_name = runtime
                    .property_names()
                    .get(property)
                    .expect("resolved runtime property name must exist");

                if let Some(value) = runtime
                    .objects
                    .get_builtin_property(handle, property_name)?
                {
                    activation.write_bytecode_register(function, instruction.a(), value)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let supports_inline_property_cache = !matches!(
                    runtime.objects.kind(handle)?,
                    HeapValueKind::Array | HeapValueKind::String
                );
                let value = if supports_inline_property_cache {
                    if let Some(cache) = frame_runtime.property_cache(function, pc) {
                        match runtime.objects.get_cached(handle, property, cache)? {
                            Some(PropertyValue::Data { value, .. }) => value,
                            Some(PropertyValue::Accessor { getter, .. }) => {
                                runtime.call_callable_for_accessor(getter, base, &[])?
                            }
                            None => Self::generic_get_property(
                                function,
                                runtime,
                                frame_runtime,
                                pc,
                                handle,
                                base,
                                property,
                            )?,
                        }
                    } else {
                        Self::generic_get_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            base,
                            property,
                        )?
                    }
                } else {
                    Self::generic_get_property(
                        function,
                        runtime,
                        frame_runtime,
                        pc,
                        handle,
                        base,
                        property,
                    )?
                };

                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetProperty => {
                let pc = activation.pc();
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let base = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_set_target_handle(base)?;
                let value = activation.read_bytecode_register(function, instruction.b())?;

                // §10.5.9 — Proxy [[Set]] trap
                if runtime.is_proxy(handle) {
                    let success = runtime.proxy_set(handle, property, value, base)?;
                    if !success && function.is_strict() {
                        let error =
                            runtime.alloc_type_error("'set' on proxy: trap returned falsish")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let primitive_base = runtime.is_primitive_property_base(base)?;

                if primitive_base {
                    let handled =
                        Self::primitive_set_property(runtime, handle, base, property, value)?;
                    if !handled && function.is_strict() {
                        let error = runtime
                            .alloc_type_error("Cannot assign to property of primitive value")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let supports_inline_property_cache = !matches!(
                    runtime.objects.kind(handle)?,
                    HeapValueKind::Array | HeapValueKind::String
                );
                let handled = if supports_inline_property_cache {
                    if let Some(cache) = frame_runtime.property_cache(function, pc) {
                        match runtime.objects.get_cached(handle, property, cache)? {
                            Some(PropertyValue::Data { .. }) => {
                                runtime.objects.set_cached(handle, property, value, cache)?
                            }
                            Some(PropertyValue::Accessor { setter, .. }) => {
                                let _ =
                                    runtime.call_callable_for_accessor(setter, base, &[value])?;
                                true
                            }
                            None => Self::generic_set_property(
                                function,
                                runtime,
                                frame_runtime,
                                pc,
                                handle,
                                base,
                                property,
                                value,
                            )?,
                        }
                    } else {
                        Self::generic_set_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            base,
                            property,
                            value,
                        )?
                    }
                } else {
                    Self::generic_set_property(
                        function,
                        runtime,
                        frame_runtime,
                        pc,
                        handle,
                        base,
                        property,
                        value,
                    )?
                };

                if !handled {
                    let cache = runtime.set_named_property(handle, property, value)?;
                    if supports_inline_property_cache {
                        frame_runtime.update_property_cache(function, pc, cache);
                    }
                }

                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::DeleteProperty => {
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                // §10.5.10 — Proxy [[Delete]] trap
                let deleted = if runtime.is_proxy(handle) {
                    runtime.proxy_delete_property(handle, property)?
                } else {
                    runtime.delete_named_property(handle, property)?
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(deleted),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::DeleteComputed => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let key_value = activation.read_bytecode_register(function, instruction.c())?;
                let property = runtime.computed_property_name(key_value)?;
                // §10.5.10 — Proxy [[Delete]] trap (computed)
                let deleted = if runtime.is_proxy(handle) {
                    runtime.proxy_delete_property(handle, property)?
                } else {
                    runtime.delete_named_property(handle, property)?
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(deleted),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetIndex => {
                let pc = activation.pc();
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let key = activation.read_bytecode_register(function, instruction.c())?;

                // §10.4.5.4 — TypedArray [[Get]] for numeric indices.
                if runtime.objects.is_typed_array(handle)
                    && let Some(index) = Self::canonical_numeric_index(key)
                {
                    let value = if index >= 0.0 && index == index.floor() {
                        runtime
                            .objects
                            .typed_array_get_element(handle, index as usize)
                            .unwrap_or(None)
                            .map(RegisterValue::from_number)
                            .unwrap_or_default()
                    } else {
                        RegisterValue::undefined()
                    };
                    activation.write_bytecode_register(function, instruction.a(), value)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let property = runtime.computed_property_name(key)?;

                // §10.5.8 — Proxy [[Get]] trap (computed)
                if runtime.is_proxy(handle) {
                    let value = runtime.proxy_get(handle, property, base)?;
                    activation.write_bytecode_register(function, instruction.a(), value)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let value = Self::generic_get_property(
                    function,
                    runtime,
                    frame_runtime,
                    pc,
                    handle,
                    base,
                    property,
                )?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetIndex => {
                let pc = activation.pc();
                let base = activation.read_bytecode_register(function, instruction.a())?;
                let key = activation.read_bytecode_register(function, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.c())?;
                let handle = runtime.property_set_target_handle(base)?;

                // §10.4.5.5 — TypedArray [[Set]] for numeric indices.
                if runtime.objects.is_typed_array(handle)
                    && let Some(index) = Self::canonical_numeric_index(key)
                {
                    if index >= 0.0 && index == index.floor() {
                        let num = runtime.js_to_number(value)?;
                        let _ =
                            runtime
                                .objects
                                .typed_array_set_element(handle, index as usize, num);
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let property = runtime.computed_property_name(key)?;

                // §10.5.9 — Proxy [[Set]] trap (computed)
                if runtime.is_proxy(handle) {
                    let success = runtime.proxy_set(handle, property, value, base)?;
                    if !success && function.is_strict() {
                        let error =
                            runtime.alloc_type_error("'set' on proxy: trap returned falsish")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let primitive_base = runtime.is_primitive_property_base(base)?;

                if primitive_base {
                    let handled =
                        Self::primitive_set_property(runtime, handle, base, property, value)?;
                    if !handled && function.is_strict() {
                        let error = runtime
                            .alloc_type_error("Cannot assign to property of primitive value")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                match runtime.objects.kind(handle)? {
                    HeapValueKind::Array => {
                        let handled = Self::generic_set_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            base,
                            property,
                            value,
                        )?;

                        if !handled {
                            runtime.set_named_property(handle, property, value)?;
                        }
                    }
                    HeapValueKind::String => {}
                    _ => {
                        let handled = Self::generic_set_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            base,
                            property,
                            value,
                        )?;

                        if !handled {
                            let cache = runtime.set_named_property(handle, property, value)?;
                            frame_runtime.update_property_cache(function, pc, cache);
                        }
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetIterator => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = base
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::TypeError("Value is not iterable".into()))?;

                // Fast path: internal iterators for Array and String.
                let iterator = match runtime.objects.alloc_iterator(handle) {
                    Ok(iterator) => iterator,
                    Err(ObjectError::InvalidKind) => {
                        // Slow path: look up Symbol.iterator method.
                        let sym_iterator = runtime.intern_symbol_property_name(
                            super::WellKnownSymbol::Iterator.stable_id(),
                        );
                        let method = runtime.ordinary_get(handle, sym_iterator, base).map_err(
                            |e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            },
                        )?;
                        let callable = method
                            .as_object_handle()
                            .map(ObjectHandle)
                            .filter(|h| runtime.objects.is_callable(*h))
                            .ok_or_else(|| {
                                InterpreterError::TypeError("Value is not iterable".into())
                            })?;
                        let iter_obj =
                            runtime
                                .call_callable(callable, base, &[])
                                .map_err(|e| match e {
                                    VmNativeCallError::Thrown(v) => {
                                        InterpreterError::UncaughtThrow(v)
                                    }
                                    VmNativeCallError::Internal(m) => {
                                        InterpreterError::NativeCall(m)
                                    }
                                })?;
                        iter_obj.as_object_handle().map(ObjectHandle).ok_or(
                            InterpreterError::TypeError(
                                "Symbol.iterator must return an object".into(),
                            ),
                        )?
                    }
                    Err(error) => return Err(error.into()),
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(iterator.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::IteratorNext => {
                let iterator = Self::read_object_handle(activation, function, instruction.c())?;
                // Fast path: internal iterators.
                let step = match runtime.iterator_next(iterator) {
                    Ok(step) => step,
                    Err(InterpreterError::InvalidHeapValueKind) => {
                        // Slow path: protocol-based iterator — call .next().
                        let next_prop = runtime.intern_property_name("next");
                        let iter_val = RegisterValue::from_object_handle(iterator.0);
                        let next_fn = runtime
                            .ordinary_get(iterator, next_prop, iter_val)
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            })?;
                        let callable = next_fn
                            .as_object_handle()
                            .map(ObjectHandle)
                            .filter(|h| runtime.objects.is_callable(*h))
                            .ok_or_else(|| {
                                InterpreterError::TypeError(
                                    "Iterator .next is not a function".into(),
                                )
                            })?;
                        let result_obj = runtime.call_callable(callable, iter_val, &[]).map_err(
                            |e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            },
                        )?;
                        let result_handle = result_obj.as_object_handle().map(ObjectHandle).ok_or(
                            InterpreterError::TypeError(
                                "Iterator .next() must return an object".into(),
                            ),
                        )?;
                        let done_prop = runtime.intern_property_name("done");
                        let done_val = runtime
                            .ordinary_get(result_handle, done_prop, result_obj)
                            .unwrap_or_else(|_| RegisterValue::from_bool(false));
                        let done = runtime.js_to_boolean(done_val).unwrap_or(false);
                        if done {
                            crate::object::IteratorStep::done()
                        } else {
                            let value_prop = runtime.intern_property_name("value");
                            let value = runtime
                                .ordinary_get(result_handle, value_prop, result_obj)
                                .unwrap_or_else(|_| RegisterValue::undefined());
                            crate::object::IteratorStep::yield_value(value)
                        }
                    }
                    Err(e) => return Err(e),
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(step.is_done()),
                )?;
                activation.write_bytecode_register(function, instruction.b(), step.value())?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §7.4.7 IteratorClose — close an iterator by calling its
            // `.return()` method if present. Built-in iterators (Array,
            // String, Map, Set) use a fast-path `closed` flag. Custom
            // iterators get a full `.return()` method call.
            // Spec: <https://tc39.es/ecma262/#sec-iteratorclose>
            Opcode::IteratorClose => {
                let iterator = Self::read_object_handle(activation, function, instruction.a())?;
                // Fast-path for built-in iterators.
                if runtime.objects.iterator_close(iterator).is_ok() {
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }
                // Custom iterator — look up `.return` and call it.
                let return_prop = runtime.intern_property_name("return");
                let iter_val = RegisterValue::from_object_handle(iterator.0);
                let return_method = match runtime.property_lookup(iterator, return_prop)? {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Data { value, .. } => value,
                        PropertyValue::Accessor { getter, .. } => {
                            runtime.call_callable_for_accessor(getter, iter_val, &[])?
                        }
                    },
                    None => RegisterValue::undefined(),
                };
                if return_method != RegisterValue::undefined()
                    && return_method != RegisterValue::null()
                    && let Some(callable) = return_method.as_object_handle().map(ObjectHandle)
                    && runtime.objects.is_callable(callable)
                {
                    // Call iterator.return() — errors propagate.
                    match runtime.call_callable(callable, iter_val, &[]) {
                        Ok(_) => {}
                        Err(VmNativeCallError::Thrown(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(VmNativeCallError::Internal(message)) => {
                            return Err(InterpreterError::NativeCall(message));
                        }
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §7.4.3 GetIterator(obj, async)
            // Spec: <https://tc39.es/ecma262/#sec-getiterator>
            //
            // 1. Let method = ? GetMethod(obj, @@asyncIterator).
            // 2. If method is undefined:
            //    a. Let syncMethod = ? GetMethod(obj, @@iterator).
            //    b. Return sync iterator (async wrapping deferred).
            // 3. Return ? GetIteratorDirect(obj, method).
            Opcode::GetAsyncIterator => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = base
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::TypeError("Value is not iterable".into()))?;

                // Step 1: Try Symbol.asyncIterator first.
                let sym_async = runtime
                    .intern_symbol_property_name(super::WellKnownSymbol::AsyncIterator.stable_id());
                let async_method =
                    runtime
                        .ordinary_get(handle, sym_async, base)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                            VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                        })?;

                let iterator = if async_method != RegisterValue::undefined()
                    && async_method != RegisterValue::null()
                {
                    // Has @@asyncIterator — call it.
                    let callable = async_method
                        .as_object_handle()
                        .map(ObjectHandle)
                        .filter(|h| runtime.objects.is_callable(*h))
                        .ok_or_else(|| {
                            InterpreterError::TypeError(
                                "Symbol.asyncIterator value is not callable".into(),
                            )
                        })?;
                    let iter_obj =
                        runtime
                            .call_callable(callable, base, &[])
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            })?;
                    iter_obj.as_object_handle().map(ObjectHandle).ok_or(
                        InterpreterError::TypeError(
                            "Symbol.asyncIterator must return an object".into(),
                        ),
                    )?
                } else {
                    // Step 2: Fall back to Symbol.iterator (sync iterator).
                    // Always use the protocol path (Symbol.iterator method call)
                    // because the compiled for-await-of loop accesses .next() via
                    // property lookup. Internal iterators from alloc_iterator have
                    // prototype: None and no protocol-accessible .next() method.
                    let sym_iterator = runtime
                        .intern_symbol_property_name(super::WellKnownSymbol::Iterator.stable_id());
                    let method = runtime
                        .ordinary_get(handle, sym_iterator, base)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                            VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                        })?;
                    let callable = method
                        .as_object_handle()
                        .map(ObjectHandle)
                        .filter(|h| runtime.objects.is_callable(*h))
                        .ok_or_else(|| {
                            InterpreterError::TypeError("Value is not async iterable".into())
                        })?;
                    let iter_obj =
                        runtime
                            .call_callable(callable, base, &[])
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            })?;
                    iter_obj.as_object_handle().map(ObjectHandle).ok_or(
                        InterpreterError::TypeError("Symbol.iterator must return an object".into()),
                    )?
                };

                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(iterator.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.2.4.1 Runtime Semantics: ArrayAccumulation — single element.
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
            //
            // Append value (register B) to the target array (register A).
            // Used when compiling array literals / argument lists with spread
            // elements, where the index is not statically known.
            Opcode::ArrayPush => {
                let target_array = Self::read_object_handle(activation, function, instruction.a())?;
                let value = activation.read_bytecode_register(function, instruction.b())?;
                runtime.objects.push_element(target_array, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.2.4.1 Runtime Semantics: ArrayAccumulation — spread.
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
            //
            // Iterate `src` (register B) via the iteration protocol and append
            // every yielded value to the target array (register A).
            Opcode::SpreadIntoArray => {
                let target_array = Self::read_object_handle(activation, function, instruction.a())?;
                let src = activation.read_bytecode_register(function, instruction.b())?;
                let src_handle = src
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::TypeError("Value is not iterable".into()))?;

                // Fast path: internal iterators for arrays and strings.
                match runtime.objects.alloc_iterator(src_handle) {
                    Ok(iterator) => loop {
                        let step = runtime.iterator_next(iterator)?;
                        if step.is_done() {
                            break;
                        }
                        runtime.objects.push_element(target_array, step.value())?;
                    },
                    Err(ObjectError::InvalidKind) => {
                        // Slow path: protocol-based iterator (Symbol.iterator).
                        let sym_iterator = runtime.intern_symbol_property_name(
                            super::WellKnownSymbol::Iterator.stable_id(),
                        );
                        let method = runtime
                            .ordinary_get(src_handle, sym_iterator, src)
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            })?;
                        let callable = method
                            .as_object_handle()
                            .map(ObjectHandle)
                            .filter(|h| runtime.objects.is_callable(*h))
                            .ok_or_else(|| {
                                InterpreterError::TypeError("Value is not iterable".into())
                            })?;
                        let iter_obj =
                            runtime
                                .call_callable(callable, src, &[])
                                .map_err(|e| match e {
                                    VmNativeCallError::Thrown(v) => {
                                        InterpreterError::UncaughtThrow(v)
                                    }
                                    VmNativeCallError::Internal(m) => {
                                        InterpreterError::NativeCall(m)
                                    }
                                })?;
                        let iter_handle = iter_obj.as_object_handle().map(ObjectHandle).ok_or(
                            InterpreterError::TypeError(
                                "Symbol.iterator must return an object".into(),
                            ),
                        )?;
                        let next_prop = runtime.intern_property_name("next");
                        let done_prop = runtime.intern_property_name("done");
                        let value_prop = runtime.intern_property_name("value");
                        loop {
                            let iter_val = RegisterValue::from_object_handle(iter_handle.0);
                            let next_fn = runtime
                                .ordinary_get(iter_handle, next_prop, iter_val)
                                .map_err(|e| match e {
                                    VmNativeCallError::Thrown(v) => {
                                        InterpreterError::UncaughtThrow(v)
                                    }
                                    VmNativeCallError::Internal(m) => {
                                        InterpreterError::NativeCall(m)
                                    }
                                })?;
                            let next_callable = next_fn
                                .as_object_handle()
                                .map(ObjectHandle)
                                .filter(|h| runtime.objects.is_callable(*h))
                                .ok_or_else(|| {
                                    InterpreterError::TypeError(
                                        "Iterator .next is not a function".into(),
                                    )
                                })?;
                            let result_obj = runtime
                                .call_callable(next_callable, iter_val, &[])
                                .map_err(|e| match e {
                                    VmNativeCallError::Thrown(v) => {
                                        InterpreterError::UncaughtThrow(v)
                                    }
                                    VmNativeCallError::Internal(m) => {
                                        InterpreterError::NativeCall(m)
                                    }
                                })?;
                            let result_handle = result_obj
                                .as_object_handle()
                                .map(ObjectHandle)
                                .ok_or(InterpreterError::TypeError(
                                    "Iterator .next() must return an object".into(),
                                ))?;
                            let done_val = runtime
                                .ordinary_get(result_handle, done_prop, result_obj)
                                .unwrap_or_else(|_| RegisterValue::from_bool(false));
                            if runtime.js_to_boolean(done_val).unwrap_or(false) {
                                break;
                            }
                            let value = runtime
                                .ordinary_get(result_handle, value_prop, result_obj)
                                .unwrap_or_else(|_| RegisterValue::undefined());
                            runtime.objects.push_element(target_array, value)?;
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.3.8.1 Runtime Semantics: ArgumentListEvaluation (spread)
            // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-argumentlistevaluation>
            //
            // Call `callee` (register B) with arguments extracted from `args_array`
            // (register C). Call metadata (construct flag, receiver) comes from the
            // call-site side table, same as CallClosure.
            //
            // Unlike CallClosure which reads args from contiguous registers, this
            // opcode reads the already-evaluated argument list from a heap array.
            // It delegates to `call_function` / `construct_callable` which handle
            // the full dispatch chain: Proxy, BoundFunction, Generator, Async,
            // Promise internal functions, HostFunction, and ordinary Closures
            // (§10.2.1, §10.3.1, §10.4.1, §10.5.12/13, §27.2, §27.3, §27.7).
            Opcode::CallSpread => {
                let call = Self::resolve_closure_call(function, activation.pc())?;
                let caller_function = module
                    .function(activation.function_index())
                    .expect("activation function index must be valid");
                let callee_value =
                    activation.read_bytecode_register(caller_function, instruction.b())?;
                let Some(callee) = callee_value.as_object_handle().map(ObjectHandle) else {
                    let error = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };

                // Extract arguments from the array built by the compiler.
                let args_array_handle =
                    Self::read_object_handle(activation, function, instruction.c())?;
                let arguments =
                    runtime
                        .objects
                        .array_elements(args_array_handle)
                        .map_err(|_| {
                            InterpreterError::TypeError("Spread arguments must be an array".into())
                        })?;

                let result = if call.flags().is_construct() {
                    // §13.3.5.1.1 EvaluateNew — construct with spread args.
                    // Spec: <https://tc39.es/ecma262/#sec-evaluatenew>
                    //
                    // §10.5.13 [[Construct]] for Proxy, §10.2.2 for ordinary,
                    // host construct for HostFunction.
                    if runtime.is_proxy(callee) {
                        runtime
                            .proxy_construct(callee, &arguments, callee)
                            .map_err(|e| match e {
                                InterpreterError::UncaughtThrow(v) => {
                                    InterpreterError::UncaughtThrow(v)
                                }
                                other => other,
                            })
                    } else if !runtime.is_constructible(callee) {
                        let error = runtime.alloc_type_error("Value is not a constructor")?;
                        Err(InterpreterError::UncaughtThrow(
                            RegisterValue::from_object_handle(error.0),
                        ))
                    } else {
                        runtime
                            .construct_callable(callee, &arguments, callee)
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            })
                    }
                } else {
                    // §13.3.8.1 — Ordinary call with spread args.
                    // Resolve receiver from call-site metadata.
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;

                    // §10.5.12 [[Call]] for Proxy.
                    if runtime.is_proxy(callee) {
                        runtime
                            .proxy_apply(callee, receiver, &arguments)
                            .map_err(|e| match e {
                                InterpreterError::UncaughtThrow(v) => {
                                    InterpreterError::UncaughtThrow(v)
                                }
                                other => other,
                            })
                    } else {
                        // call_function handles: Closure (ordinary, async, generator,
                        // class constructor guard), BoundFunction, HostFunction,
                        // PromiseCapabilityFunction, PromiseCombinatorElement,
                        // PromiseFinallyFunction, PromiseValueThunk.
                        Self::call_function(runtime, module, callee, receiver, &arguments)
                    }
                };

                match result {
                    Ok(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.write_bytecode_register(function, instruction.a(), value)?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(InterpreterError::UncaughtThrow(value)) => Ok(StepOutcome::Throw(value)),
                    Err(error) => Err(error),
                }
            }
            // V8-style LdaGlobal: load a global variable by name from the
            // global object (receiver r0).  Throws if not found.
            Opcode::GetGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let global_handle = runtime.intrinsics().global_object();
                let value = runtime.objects.get_property(global_handle, property)?;
                match value {
                    Some(lookup) => {
                        let val = match lookup.value() {
                            PropertyValue::Data { value: v, .. } => v,
                            PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                        };
                        activation.write_bytecode_register(function, instruction.a(), val)?;
                    }
                    None => {
                        // Property not found → throw (ReferenceError semantics).
                        let name = runtime.property_names().get(property).unwrap_or("?");
                        let msg = format!("{name} is not defined");
                        let error_obj = runtime.alloc_reference_error(&msg)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error_obj.0,
                        )));
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.a())?;
                let global_handle = runtime.intrinsics().global_object();
                runtime
                    .objects
                    .set_property(global_handle, property, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Strict-mode SetGlobal: throws ReferenceError if the property
            // does not already exist on the global object (including prototype
            // chain). This implements ES2024 §6.2.5.6 PutValue step 5.a:
            // "If IsUnresolvableReference(V) is true, throw a ReferenceError."
            Opcode::SetGlobalStrict => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let global_handle = runtime.intrinsics().global_object();
                // Check if the property exists on the global object (own or inherited).
                let exists = runtime.objects.get_property(global_handle, property)?;
                if exists.is_none() {
                    let name = runtime.property_names().get(property).unwrap_or("?");
                    let msg = format!("{name} is not defined");
                    let error_obj = runtime.alloc_reference_error(&msg)?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error_obj.0,
                    )));
                }
                let value = activation.read_bytecode_register(function, instruction.a())?;
                runtime
                    .objects
                    .set_property(global_handle, property, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // typeof on a global variable — returns "undefined" for unresolvable.
            // ES2024 §13.5.1: typeof on an unresolvable Reference returns "undefined".
            Opcode::TypeOfGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let global_handle = runtime.intrinsics().global_object();
                let value = runtime.objects.get_property(global_handle, property)?;
                let val = match value {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Data { value: v, .. } => v,
                        PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                    },
                    None => RegisterValue::undefined(),
                };
                let type_val = runtime.js_typeof(val)?;
                activation.write_bytecode_register(function, instruction.a(), type_val)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetPropertyIterator => {
                let object_val = activation.read_bytecode_register(function, instruction.b())?;
                // ES spec 13.7.5.15: for-in on null/undefined produces no iterations.
                // Primitives (number, bool) have no enumerable own properties.
                let iter_handle = if object_val == RegisterValue::null()
                    || object_val == RegisterValue::undefined()
                {
                    runtime.alloc_empty_property_iterator()?
                } else if let Some(handle) = object_val.as_object_handle().map(ObjectHandle) {
                    runtime.alloc_property_iterator(handle)?
                } else {
                    runtime.alloc_empty_property_iterator()?
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(iter_handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::PropertyIteratorNext => {
                let iter_val = activation.read_bytecode_register(function, instruction.c())?;
                let iter_handle = iter_val
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let step = runtime.objects.property_iterator_next(iter_handle)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(step.is_done()),
                )?;
                activation.write_bytecode_register(function, instruction.b(), step.value())?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // ES spec §7.3.21 OrdinaryHasInstance — `lhs instanceof rhs`.
            Opcode::InstanceOf => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let result = match runtime.js_instance_of(lhs, rhs) {
                    Ok(result) => result,
                    Err(InterpreterError::TypeError(message)) => {
                        let error = runtime.alloc_type_error(&message)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    Err(error) => return Err(error),
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // `in` operator — check if property exists on object.
            Opcode::HasProperty => {
                let key = activation.read_bytecode_register(function, instruction.b())?;
                let object = activation.read_bytecode_register(function, instruction.c())?;
                let result = match runtime.js_has_property(key, object) {
                    Ok(result) => result,
                    Err(InterpreterError::TypeError(message)) => {
                        let error = runtime.alloc_type_error(&message)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    Err(error) => return Err(error),
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetUpvalue => {
                let upvalue =
                    Self::resolve_upvalue_cell(activation, runtime, UpvalueId(instruction.b()))?;
                let value = runtime.objects.get_upvalue(upvalue)?;
                if value.is_hole() {
                    let error =
                        runtime.alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetUpvalue => {
                let upvalue =
                    Self::resolve_upvalue_cell(activation, runtime, UpvalueId(instruction.b()))?;
                let value = activation.read_bytecode_register(function, instruction.a())?;
                runtime.objects.set_upvalue(upvalue, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CallDirect => {
                let call = Self::resolve_direct_call(function, activation.pc())?;
                let mut callee_activation =
                    Self::prepare_direct_call(module, function, activation, instruction.b(), call)?;
                match self.run_completion_with_runtime(module, &mut callee_activation, runtime)? {
                    Completion::Return(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.write_bytecode_register(function, instruction.a(), value)?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                }
            }
            Opcode::CallClosure => {
                let call = Self::resolve_closure_call(function, activation.pc())?;
                let caller_function = module
                    .function(activation.function_index())
                    .expect("activation function index must be valid");
                let callee_value =
                    activation.read_bytecode_register(caller_function, instruction.b())?;
                let Some(callee) = callee_value.as_object_handle().map(ObjectHandle) else {
                    let error = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };

                // ES2024 §10.4.1.1 [[Call]] — resolve bound function before dispatch.
                let arguments = Self::read_call_arguments(
                    caller_function,
                    activation,
                    instruction.c(),
                    call.argument_count(),
                )?;

                // §10.5.12/§10.5.13 — Proxy [[Call]]/[[Construct]] trap
                if runtime.is_proxy(callee) {
                    if call.flags().is_construct() {
                        match runtime.proxy_construct(callee, &arguments, callee) {
                            Ok(value) => {
                                activation.refresh_open_upvalues_from_cells(runtime)?;
                                activation.write_bytecode_register(
                                    function,
                                    instruction.a(),
                                    value,
                                )?;
                                activation.advance();
                                return Ok(StepOutcome::Continue);
                            }
                            Err(InterpreterError::UncaughtThrow(value)) => {
                                return Ok(StepOutcome::Throw(value));
                            }
                            Err(error) => return Err(error),
                        }
                    } else {
                        let receiver = Self::resolve_call_receiver(
                            caller_function,
                            activation,
                            call.flags(),
                            call.receiver(),
                            None,
                        )?;
                        match runtime.proxy_apply(callee, receiver, &arguments) {
                            Ok(value) => {
                                activation.refresh_open_upvalues_from_cells(runtime)?;
                                activation.write_bytecode_register(
                                    function,
                                    instruction.a(),
                                    value,
                                )?;
                                activation.advance();
                                return Ok(StepOutcome::Continue);
                            }
                            Err(InterpreterError::UncaughtThrow(value)) => {
                                return Ok(StepOutcome::Throw(value));
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }

                if call.flags().is_construct() {
                    if !runtime.is_constructible(callee) {
                        let error = runtime.alloc_type_error("Value is not a constructor")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    match runtime.construct_callable(callee, &arguments, callee) {
                        Ok(value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Err(VmNativeCallError::Thrown(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(VmNativeCallError::Internal(message)) => {
                            return Err(InterpreterError::NativeCall(message));
                        }
                    }
                }

                if !runtime.objects.is_callable(callee) {
                    let error = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }

                if matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                    && runtime
                        .objects
                        .closure_flags(callee)
                        .is_ok_and(|flags| flags.is_class_constructor())
                {
                    let error = runtime
                        .alloc_type_error("Class constructor cannot be invoked without 'new'")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }

                // §27.6.3.1 — Async generator function call: create an async
                // generator object instead of executing the body.
                // Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorstart>
                if matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                    && runtime
                        .objects
                        .closure_flags(callee)
                        .is_ok_and(|flags| flags.is_generator() && flags.is_async())
                {
                    let callee_module = runtime.objects.closure_module(callee)?;
                    let callee_fn_index = runtime.objects.closure_callee(callee)?;
                    let gen_handle = runtime.alloc_async_generator(
                        callee_module,
                        callee_fn_index,
                        Some(callee),
                        arguments.clone(),
                    );

                    // §15.8.3 step 1-2: Run param init eagerly. The implicit
                    // initial yield suspends after param init is complete.
                    match Self::run_async_generator_param_init(runtime, gen_handle) {
                        Ok(()) => {}
                        Err(VmNativeCallError::Thrown(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(VmNativeCallError::Internal(msg)) => {
                            return Err(InterpreterError::NativeCall(msg));
                        }
                    }

                    activation.write_bytecode_register(
                        function,
                        instruction.a(),
                        RegisterValue::from_object_handle(gen_handle.0),
                    )?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                // §27.3.3.1 / §15.5.2 — Generator function call: create a
                // generator object and eagerly execute FunctionDeclarationInstantiation
                // (parameter initialization). The compiler emits an implicit initial
                // Yield after param init, so `resume_generator_impl` runs param init
                // and suspends at that yield. If param init throws, the error
                // propagates to the caller (not to the first `.next()`).
                // Spec: <https://tc39.es/ecma262/#sec-generatorfunction-objects-call>
                if matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                    && runtime
                        .objects
                        .closure_flags(callee)
                        .is_ok_and(|flags| flags.is_generator())
                {
                    let callee_module = runtime.objects.closure_module(callee)?;
                    let callee_fn_index = runtime.objects.closure_callee(callee)?;
                    let gen_handle = runtime.alloc_generator(
                        callee_module,
                        callee_fn_index,
                        Some(callee),
                        arguments.clone(),
                    );

                    // §15.5.2 step 1-2: Run param init eagerly. The implicit
                    // initial yield (emitted by the compiler) suspends execution
                    // after param init is complete. Discard the yielded result
                    // (`{value: undefined, done: false}`).
                    match Self::resume_generator_impl(
                        runtime,
                        gen_handle,
                        RegisterValue::undefined(),
                        crate::intrinsics::GeneratorResumeKind::Next,
                    ) {
                        Ok(_implicit_yield_result) => {
                            // Param init succeeded; generator suspended at
                            // implicit yield. First `.next()` resumes body.
                        }
                        Err(VmNativeCallError::Thrown(value)) => {
                            // Param init threw — propagate to caller.
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(VmNativeCallError::Internal(msg)) => {
                            return Err(InterpreterError::NativeCall(msg));
                        }
                    }

                    activation.write_bytecode_register(
                        function,
                        instruction.a(),
                        RegisterValue::from_object_handle(gen_handle.0),
                    )?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                // §27.7.5.1 — Async function call: execute the body and wrap
                // the result in a Promise.
                // Spec: <https://tc39.es/ecma262/#sec-async-functions-abstract-operations-async-function-start>
                if matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                    && runtime
                        .objects
                        .closure_flags(callee)
                        .is_ok_and(|flags| flags.is_async())
                {
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;
                    match Self::call_function(runtime, module, callee, receiver, &arguments) {
                        Ok(promise_value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(
                                function,
                                instruction.a(),
                                promise_value,
                            )?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Err(InterpreterError::UncaughtThrow(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(error) => return Err(error),
                    }
                }

                if let Ok(HeapValueKind::BoundFunction) = runtime.objects.kind(callee) {
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;
                    match runtime.call_callable_for_accessor(Some(callee), receiver, &arguments) {
                        Ok(value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Err(InterpreterError::UncaughtThrow(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(error) => return Err(error),
                    }
                }

                // ES2024 §27.2.1.3 — Promise capability / combinator / finally functions.
                if matches!(
                    runtime.objects.kind(callee),
                    Ok(HeapValueKind::PromiseCapabilityFunction
                        | HeapValueKind::PromiseCombinatorElement
                        | HeapValueKind::PromiseFinallyFunction
                        | HeapValueKind::PromiseValueThunk)
                ) {
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;
                    match Self::invoke_host_function_handle(runtime, callee, receiver, &arguments)?
                    {
                        Completion::Return(value) => {
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Completion::Throw(value) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                    }
                }

                if let Some(host_function) = runtime.objects.host_function(callee)? {
                    match Self::invoke_host_function(
                        callee,
                        caller_function,
                        activation,
                        runtime,
                        host_function,
                        instruction.c(),
                        call,
                    )? {
                        Completion::Return(value) => {
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                    }
                } else {
                    let (callee_module, mut callee_activation) = Self::prepare_closure_call(
                        module,
                        activation,
                        runtime,
                        instruction.b(),
                        instruction.c(),
                        call,
                    )?;
                    match self.run_completion_with_runtime(
                        &callee_module,
                        &mut callee_activation,
                        runtime,
                    )? {
                        Completion::Return(value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                    }
                }
            }
            // §14.6 Tail Position Calls — reuse the current frame.
            // The execution loop in `run_completion_with_runtime` handles
            // `StepOutcome::TailCall` by swapping module/activation in-place.
            // Spec: <https://tc39.es/ecma262/#sec-tail-position-calls>
            Opcode::TailCallClosure => {
                let call = Self::resolve_closure_call(function, activation.pc())?;
                let caller_function = module
                    .function(activation.function_index())
                    .expect("activation function index must be valid");
                let callee_value =
                    activation.read_bytecode_register(caller_function, instruction.a())?;
                let Some(callee) = callee_value.as_object_handle().map(ObjectHandle) else {
                    let error = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };

                // For non-closure callables (native, proxy, bound, host, etc.)
                // fall back to a regular call + return — TCO only applies to
                // bytecode closures.
                let is_plain_closure =
                    matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                        && !runtime.objects.closure_flags(callee).is_ok_and(|f| {
                            f.is_generator() || f.is_async() || f.is_class_constructor()
                        })
                        && runtime.objects.host_function(callee)?.is_none();

                if is_plain_closure {
                    // Prepare callee activation and return TailCall to the loop.
                    let (callee_module, callee_activation) = Self::prepare_closure_call(
                        module,
                        activation,
                        runtime,
                        instruction.a(),
                        instruction.b(),
                        call,
                    )?;
                    Ok(StepOutcome::TailCall(Box::new(TailCallPayload {
                        module: callee_module,
                        activation: callee_activation,
                    })))
                } else {
                    // Non-closure target: execute as normal call, then return
                    // the result from this frame.
                    let arguments = Self::read_call_arguments(
                        caller_function,
                        activation,
                        instruction.b(),
                        call.argument_count(),
                    )?;
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;

                    if runtime.is_proxy(callee) {
                        match runtime.proxy_apply(callee, receiver, &arguments) {
                            Ok(value) => Ok(StepOutcome::Return(value)),
                            Err(InterpreterError::UncaughtThrow(value)) => {
                                Ok(StepOutcome::Throw(value))
                            }
                            Err(error) => Err(error),
                        }
                    } else if let Some(host_function) = runtime.objects.host_function(callee)? {
                        match Self::invoke_host_function(
                            callee,
                            caller_function,
                            activation,
                            runtime,
                            host_function,
                            instruction.b(),
                            call,
                        )? {
                            Completion::Return(value) => Ok(StepOutcome::Return(value)),
                            Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                        }
                    } else {
                        // Bound function or other exotic: regular call path.
                        let result = runtime.call_callable(callee, receiver, &arguments);
                        match result {
                            Ok(value) => Ok(StepOutcome::Return(value)),
                            Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                            Err(VmNativeCallError::Internal(message)) => {
                                Err(InterpreterError::NativeCall(message))
                            }
                        }
                    }
                }
            }
            Opcode::CallSuper => {
                // Resolve the effective derived-constructor closure and the
                // new-target to forward to the super constructor. For direct
                // usage inside a derived constructor body these come from the
                // active function/activation. For `() => super()` inside the
                // same body, walk the arrow's `lexical_parent_closure` chain
                // up to the enclosing non-arrow closure (which must be a
                // derived class constructor) and use its captured new-target.
                // Spec: <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>
                let current_closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let (effective_closure, new_target) = if function.is_derived_constructor()
                    && activation.metadata().flags().is_construct()
                {
                    (
                        current_closure,
                        activation.construct_new_target().unwrap_or(current_closure),
                    )
                } else if let Some(parent) = runtime
                    .objects
                    .closure_lexical_non_arrow_ancestor(current_closure)?
                    && runtime
                        .objects
                        .closure_flags(parent)?
                        .is_class_constructor()
                    && let Some(captured_nt) = runtime
                        .objects
                        .closure_captured_new_target(current_closure)?
                {
                    (parent, captured_nt)
                } else {
                    let error = runtime.alloc_reference_error("'super' keyword unexpected here")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let Some(super_ctor) = runtime.objects.get_prototype(effective_closure)? else {
                    let error = runtime.alloc_type_error("Super constructor is not available")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let argc = instruction.c();
                let mut arguments = Vec::with_capacity(usize::from(argc));
                for offset in 0..argc {
                    let value = activation
                        .read_bytecode_register(function, instruction.b().saturating_add(offset))?;
                    arguments.push(value);
                }

                match runtime.construct_callable(super_ctor, &arguments, new_target) {
                    Ok(this_value) => {
                        // §13.3.7.1 SuperCall step 8 / §9.1.1.3.1
                        // BindThisValue — the super constructor has already
                        // run, but we must refuse to (re-)bind `this` when
                        // the derived constructor already initialised it.
                        // Spec: <https://tc39.es/ecma262/#sec-bindthisvalue>
                        if function.frame_layout().receiver_slot().is_some()
                            && function.is_derived_constructor()
                            && activation.metadata().flags().is_construct()
                        {
                            let current_this = activation.receiver(function)?;
                            if current_this != RegisterValue::undefined() && !current_this.is_hole()
                            {
                                let error = runtime.alloc_reference_error(
                                    "Super constructor may only be called once",
                                )?;
                                return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                                    error.0,
                                )));
                            }
                        }
                        if function.frame_layout().receiver_slot().is_some() {
                            activation.set_receiver(function, this_value)?;
                        }
                        activation.write_bytecode_register(
                            function,
                            instruction.a(),
                            this_value,
                        )?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
            // §12.3.7.1 SuperCall — spread variant.
            // Spec: <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>
            //
            // Same semantics as CallSuper but reads arguments from an array
            // register (B) instead of a contiguous register window.
            Opcode::CallSuperSpread => {
                let current_closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let (effective_closure, new_target) = if function.is_derived_constructor()
                    && activation.metadata().flags().is_construct()
                {
                    (
                        current_closure,
                        activation.construct_new_target().unwrap_or(current_closure),
                    )
                } else if let Some(parent) = runtime
                    .objects
                    .closure_lexical_non_arrow_ancestor(current_closure)?
                    && runtime
                        .objects
                        .closure_flags(parent)?
                        .is_class_constructor()
                    && let Some(captured_nt) = runtime
                        .objects
                        .closure_captured_new_target(current_closure)?
                {
                    (parent, captured_nt)
                } else {
                    let error = runtime.alloc_reference_error("'super' keyword unexpected here")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let Some(super_ctor) = runtime.objects.get_prototype(effective_closure)? else {
                    let error = runtime.alloc_type_error("Super constructor is not available")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };

                let args_array_handle =
                    Self::read_object_handle(activation, function, instruction.b())?;
                let arguments =
                    runtime
                        .objects
                        .array_elements(args_array_handle)
                        .map_err(|_| {
                            InterpreterError::TypeError("Spread arguments must be an array".into())
                        })?;

                match runtime.construct_callable(super_ctor, &arguments, new_target) {
                    Ok(this_value) => {
                        if function.frame_layout().receiver_slot().is_some() {
                            activation.set_receiver(function, this_value)?;
                        }
                        activation.write_bytecode_register(
                            function,
                            instruction.a(),
                            this_value,
                        )?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
            Opcode::CallSuperForward => {
                let current_closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let (effective_closure, new_target) = if function.is_derived_constructor()
                    && activation.metadata().flags().is_construct()
                {
                    (
                        current_closure,
                        activation.construct_new_target().unwrap_or(current_closure),
                    )
                } else if let Some(parent) = runtime
                    .objects
                    .closure_lexical_non_arrow_ancestor(current_closure)?
                    && runtime
                        .objects
                        .closure_flags(parent)?
                        .is_class_constructor()
                    && let Some(captured_nt) = runtime
                        .objects
                        .closure_captured_new_target(current_closure)?
                {
                    (parent, captured_nt)
                } else {
                    let error = runtime.alloc_reference_error("'super' keyword unexpected here")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let Some(super_ctor) = runtime.objects.get_prototype(effective_closure)? else {
                    let error = runtime.alloc_type_error("Super constructor is not available")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let param_count = function.frame_layout().parameter_count();
                let actual_argc = activation.metadata().argument_count();
                let mut arguments = Vec::with_capacity(usize::from(actual_argc));
                for offset in 0..actual_argc {
                    let value = if offset < param_count {
                        activation.read_bytecode_register(function, offset)?
                    } else {
                        *activation
                            .overflow_args
                            .get(usize::from(offset - param_count))
                            .ok_or(InterpreterError::RegisterOutOfBounds)?
                    };
                    arguments.push(value);
                }

                match runtime.construct_callable(super_ctor, &arguments, new_target) {
                    Ok(this_value) => {
                        if function.frame_layout().receiver_slot().is_some() {
                            activation.set_receiver(function, this_value)?;
                        }
                        activation.write_bytecode_register(
                            function,
                            instruction.a(),
                            this_value,
                        )?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
            Opcode::Jump => {
                let offset = instruction.immediate_i32();
                if offset < 0 {
                    self.check_interrupt()?;
                    runtime.gc_safepoint(activation.registers());
                }
                activation.jump_relative(offset)?;
                Ok(StepOutcome::Continue)
            }
            Opcode::JumpIfTrue => {
                let condition = activation.read_bytecode_register(function, instruction.a())?;
                if runtime.js_to_boolean(condition)? {
                    let offset = instruction.immediate_i32();
                    if offset < 0 {
                        self.check_interrupt()?;
                        runtime.gc_safepoint(activation.registers());
                    }
                    activation.jump_relative(offset)?;
                } else {
                    activation.advance();
                }
                Ok(StepOutcome::Continue)
            }
            Opcode::JumpIfFalse => {
                let condition = activation.read_bytecode_register(function, instruction.a())?;
                if runtime.js_to_boolean(condition)? {
                    activation.advance();
                } else {
                    let offset = instruction.immediate_i32();
                    if offset < 0 {
                        self.check_interrupt()?;
                        runtime.gc_safepoint(activation.registers());
                    }
                    activation.jump_relative(offset)?;
                }
                Ok(StepOutcome::Continue)
            }
            Opcode::Return => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                Ok(StepOutcome::Return(value))
            }
            Opcode::Throw => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                Ok(StepOutcome::Throw(value))
            }
            Opcode::Await => {
                let dst_reg = instruction.a();
                let src_reg = instruction.b();
                let value = activation.read_bytecode_register(function, src_reg)?;

                // Check if the value is an already-settled promise.
                // If it's an object handle, look it up as a JsPromise.
                if let Some(handle_id) = value.as_object_handle() {
                    let handle = ObjectHandle(handle_id);
                    // Try to read as JsPromise from the typed heap.
                    if let Some(promise) = runtime.objects().get_promise(handle) {
                        match &promise.state {
                            crate::promise::PromiseState::Fulfilled(result) => {
                                // Already fulfilled — write result, continue.
                                let result = *result;
                                let abs =
                                    activation.resolve_bytecode_register(function, dst_reg)?;
                                activation.set_register(abs, result)?;
                                activation.advance();
                                return Ok(StepOutcome::Continue);
                            }
                            crate::promise::PromiseState::Rejected(reason) => {
                                // Already rejected — throw the reason.
                                // Do NOT advance the PC: transfer_exception needs
                                // the PC at the Await instruction to find the
                                // enclosing try/catch handler.
                                let reason = *reason;
                                return Ok(StepOutcome::Throw(reason));
                            }
                            crate::promise::PromiseState::Pending => {
                                // Pending — suspend.
                                let abs =
                                    activation.resolve_bytecode_register(function, dst_reg)?;
                                activation.advance();
                                return Ok(StepOutcome::Suspend {
                                    awaited_promise: handle,
                                    resume_register: abs,
                                });
                            }
                        }
                    }
                }

                // Not a promise — treat as immediately fulfilled with the value itself.
                let abs = activation.resolve_bytecode_register(function, dst_reg)?;
                activation.set_register(abs, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §14.4 Yield — suspend generator and produce a value.
            // Spec: <https://tc39.es/ecma262/#sec-yield>
            Opcode::Yield => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let resume_reg = instruction.a();
                // Advance PC past the Yield instruction so resume continues
                // at the next instruction.
                activation.advance();
                Ok(StepOutcome::GeneratorYield {
                    yielded_value: value,
                    resume_register: resume_reg,
                })
            }
            // §14.4.4 yield* — delegate to a sub-iterator.
            // Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
            Opcode::YieldStar => {
                let dst_reg = instruction.a();
                let iterator_reg = instruction.b();
                let iterator_value = activation.read_bytecode_register(function, iterator_reg)?;

                let iterator_handle = iterator_value
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or_else(|| {
                        InterpreterError::TypeError("yield* operand is not an object".into())
                    })?;

                // Call inner.next(undefined) to get the first result.
                let (done, value) = runtime
                    .call_iterator_next_with_value(iterator_handle, RegisterValue::undefined())?;

                if done {
                    // Inner iterator immediately done — write return value to dst.
                    activation.write_bytecode_register(function, dst_reg, value)?;
                    activation.advance();
                    Ok(StepOutcome::Continue)
                } else {
                    // Store pending delegation for the resume loop to pick up.
                    runtime.pending_delegation_iterator = Some(iterator_handle);
                    activation.advance();
                    Ok(StepOutcome::GeneratorYield {
                        yielded_value: value,
                        resume_register: dst_reg,
                    })
                }
            }
            // §13.3.10 Dynamic import() — evaluate specifier and return a Promise.
            // Spec: <https://tc39.es/ecma262/#sec-import-calls>
            Opcode::DynamicImport => {
                let dst_reg = instruction.a();
                let specifier_reg = instruction.b();
                let specifier_value = activation.read_bytecode_register(function, specifier_reg)?;

                // Coerce specifier to string.
                let specifier_str = runtime.js_to_string(specifier_value)?;

                // Look up the host-installed __importDynamic function on the global.
                let prop = runtime.intern_property_name("__importDynamic");
                let global = runtime.intrinsics().global_object();
                let handler_value = runtime.own_property_value(global, prop).unwrap_or_default();

                let result = if let Some(handle_id) = handler_value.as_object_handle() {
                    // Call __importDynamic(specifier) and return its result
                    // (should be a Promise).
                    let specifier_handle = runtime.alloc_string(specifier_str);
                    let specifier_rv = RegisterValue::from_object_handle(specifier_handle.0);
                    runtime.call_callable_for_accessor(
                        Some(ObjectHandle(handle_id)),
                        RegisterValue::undefined(),
                        &[specifier_rv],
                    )?
                } else {
                    // No host handler — throw a TypeError.
                    return Err(InterpreterError::TypeError(
                        "import() requires a host-installed __importDynamic handler".into(),
                    ));
                };

                activation.write_bytecode_register(function, dst_reg, result)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §13.3.12 import.meta — return a module metadata object.
            // Spec: <https://tc39.es/ecma262/#sec-meta-properties>
            Opcode::ImportMeta => {
                let dst_reg = instruction.a();

                // Build { url: "<module name>" } object.
                let module_url: Option<Box<str>> = runtime
                    .current_module
                    .as_ref()
                    .and_then(|m| m.name().map(|n| n.into()));
                let meta_object = runtime.alloc_object();
                let url_prop = runtime.intern_property_name("url");
                let url_value = if let Some(url) = module_url {
                    let handle = runtime.alloc_string(url);
                    RegisterValue::from_object_handle(handle.0)
                } else {
                    RegisterValue::undefined()
                };
                runtime
                    .objects_mut()
                    .set_property(meta_object, url_prop, url_value)
                    .map_err(|_| {
                        InterpreterError::TypeError("cannot set import.meta.url".into())
                    })?;

                let result = RegisterValue::from_object_handle(meta_object.0);
                activation.write_bytecode_register(function, dst_reg, result)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }

            // §19.2.1.1 PerformEval — direct eval.
            // `CallEval dst, code`
            // If code is not a string, returns it unchanged.
            // Otherwise compiles and executes the source in the current
            // runtime, inheriting the caller's `this`, HomeObject, and
            // PrivateNameEnvironment so that `this.#f`, `super.x`, and
            // `this` references work inside the eval'd code.
            // Spec: <https://tc39.es/ecma262/#sec-performeval>
            Opcode::CallEval => {
                let dst_reg = instruction.a();
                let code_reg = instruction.b();
                let code_value = activation.read_bytecode_register(function, code_reg)?;

                // §19.2.1 Step 1: If x is not a String, return x.
                let result = if let Some(source) = runtime.value_as_string(code_value) {
                    // Capture caller context for private names and super access.
                    let caller_closure = activation.closure_handle();
                    let caller_this = activation.receiver(function).unwrap_or_default();
                    // §19.2.1.1 PerformEval(x, strictCaller, direct=true) with
                    // caller's closure context for private/super/this.
                    Self::eval_source_direct(runtime, &source, caller_closure, caller_this)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(value) => {
                                InterpreterError::UncaughtThrow(value)
                            }
                            VmNativeCallError::Internal(msg) => InterpreterError::TypeError(msg),
                        })?
                } else {
                    code_value
                };

                activation.write_bytecode_register(function, dst_reg, result)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
        }
    }

    fn resolve_property_name(
        function: &Function,
        runtime: &mut RuntimeState,
        raw_id: RegisterIndex,
    ) -> Result<PropertyNameId, InterpreterError> {
        let property_name = function
            .property_names()
            .get(PropertyNameId(raw_id))
            .ok_or(InterpreterError::UnknownPropertyName)?;
        Ok(runtime.intern_property_name(property_name))
    }

    /// §6.2.12 — Resolve a property-name operand into a PrivateNameKey by combining it
    /// with the current closure's class_id.
    /// Resolves the class_id for a private member operation.
    ///
    /// Tries the current closure first; if unavailable or class_id is 0, falls
    /// back to reading class_id from a fallback object (typically the constructor).
    /// This is needed because static private element definitions run in the outer
    /// function context, not inside the constructor closure.
    fn resolve_class_id(
        activation: &Activation,
        runtime: &RuntimeState,
        fallback_object: Option<ObjectHandle>,
    ) -> Result<u64, InterpreterError> {
        if let Some(closure) = activation.closure_handle() {
            let id = runtime.objects.closure_class_id(closure).unwrap_or(0);
            if id != 0 {
                return Ok(id);
            }
        }
        // Fallback: read class_id from the target object (constructor).
        if let Some(obj) = fallback_object {
            let id = runtime.objects.closure_class_id(obj).unwrap_or(0);
            if id != 0 {
                return Ok(id);
            }
        }
        Err(InterpreterError::MissingClosureContext)
    }

    fn resolve_private_name_key(
        function: &Function,
        _runtime: &mut RuntimeState,
        raw_id: RegisterIndex,
        class_id: u64,
    ) -> Result<crate::object::PrivateNameKey, InterpreterError> {
        let property_name_str = function
            .property_names()
            .get(PropertyNameId(raw_id))
            .ok_or(InterpreterError::UnknownPropertyName)?;
        Ok(crate::object::PrivateNameKey {
            class_id,
            description: property_name_str.into(),
        })
    }

    fn resolve_string_literal(
        function: &Function,
        raw_id: RegisterIndex,
    ) -> Result<crate::js_string::JsString, InterpreterError> {
        function
            .string_literals()
            .get_js(StringId(raw_id))
            .cloned()
            .ok_or(InterpreterError::UnknownStringLiteral)
    }

    /// Resolves a RegExp-literal entry from the function's regexp side table.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-literals-regular-expression-literals>
    fn resolve_regexp_literal(
        function: &Function,
        raw_id: RegisterIndex,
    ) -> Result<&crate::regexp::RegExpEntry, InterpreterError> {
        function
            .regexp_literals()
            .get(crate::regexp::RegExpId(raw_id))
            .ok_or(InterpreterError::UnknownStringLiteral)
    }

    fn resolve_closure_template(
        function: &Function,
        pc: ProgramCounter,
    ) -> Result<ClosureTemplate, InterpreterError> {
        function
            .closures()
            .get(pc)
            .ok_or(InterpreterError::UnknownClosureTemplate)
    }

    fn resolve_direct_call(
        function: &Function,
        pc: ProgramCounter,
    ) -> Result<DirectCall, InterpreterError> {
        function
            .calls()
            .get_direct(pc)
            .ok_or(InterpreterError::UnknownCallSite)
    }

    fn resolve_closure_call(
        function: &Function,
        pc: ProgramCounter,
    ) -> Result<ClosureCall, InterpreterError> {
        function
            .calls()
            .get_closure(pc)
            .ok_or(InterpreterError::UnknownCallSite)
    }

    fn read_object_handle(
        activation: &Activation,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<ObjectHandle, InterpreterError> {
        let value = activation.read_bytecode_register(function, register)?;
        value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(InterpreterError::InvalidObjectValue)
    }

    fn generic_get_property(
        function: &Function,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
        pc: ProgramCounter,
        handle: ObjectHandle,
        receiver: RegisterValue,
        property: PropertyNameId,
    ) -> Result<RegisterValue, InterpreterError> {
        match runtime.property_lookup(handle, property)? {
            Some(lookup) => {
                if let Some(cache) = lookup.cache() {
                    frame_runtime.update_property_cache(function, pc, cache);
                }
                match lookup.value() {
                    PropertyValue::Data { value, .. } => Ok(value),
                    PropertyValue::Accessor { getter, .. } => {
                        runtime.call_callable_for_accessor(getter, receiver, &[])
                    }
                }
            }
            None => Ok(RegisterValue::undefined()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn generic_set_property(
        function: &Function,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
        pc: ProgramCounter,
        handle: ObjectHandle,
        receiver: RegisterValue,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        match runtime.objects.kind(handle)? {
            HeapValueKind::String => return Ok(false),
            HeapValueKind::Array => {
                return match runtime.property_lookup(handle, property)? {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Accessor { setter, .. } => {
                            let _ =
                                runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                            Ok(true)
                        }
                        PropertyValue::Data { .. } => Ok(false),
                    },
                    None => Ok(false),
                };
            }
            _ => {}
        }

        match runtime.property_lookup(handle, property)? {
            Some(lookup) => {
                if let Some(cache) = lookup.cache() {
                    frame_runtime.update_property_cache(function, pc, cache);
                }
                match lookup.value() {
                    PropertyValue::Data { .. } if lookup.owner() == handle => {
                        if let Some(cache) = lookup.cache() {
                            let updated =
                                runtime.objects.set_cached(handle, property, value, cache)?;
                            if updated {
                                return Ok(true);
                            }
                        }
                        let cache = runtime.objects.set_property(handle, property, value)?;
                        frame_runtime.update_property_cache(function, pc, cache);
                        Ok(true)
                    }
                    PropertyValue::Data { .. } => Ok(false),
                    PropertyValue::Accessor { setter, .. } => {
                        let _ = runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                        Ok(true)
                    }
                }
            }
            None => Ok(false),
        }
    }

    fn primitive_set_property(
        runtime: &mut RuntimeState,
        target: ObjectHandle,
        receiver: RegisterValue,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        match runtime.property_lookup(target, property)? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Accessor { setter, .. } => {
                    let _ = runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                    Ok(true)
                }
                PropertyValue::Data { .. } => Ok(false),
            },
            None => Ok(false),
        }
    }

    /// §7.1.21 CanonicalNumericIndexString
    /// <https://tc39.es/ecma262/#sec-canonicalnumericindexstring>
    ///
    /// Returns `Some(n)` if `key` represents a canonical numeric index (integer
    /// encoded as an i32/f64 value, or a string that converts to a number
    /// whose `ToString` matches the original string). Used by TypedArray
    /// [[Get]]/[[Set]] to intercept numeric property access.
    fn canonical_numeric_index(key: RegisterValue) -> Option<f64> {
        if let Some(n) = key.as_i32() {
            return Some(f64::from(n));
        }
        if let Some(n) = key.as_number()
            && !n.is_nan()
        {
            return Some(n);
        }
        None
    }

    fn resolve_upvalue_cell(
        activation: &Activation,
        runtime: &RuntimeState,
        upvalue: UpvalueId,
    ) -> Result<ObjectHandle, InterpreterError> {
        let closure = activation
            .closure_handle()
            .ok_or(InterpreterError::MissingClosureContext)?;
        runtime
            .objects
            .closure_upvalue(closure, usize::from(upvalue.0))
            .map_err(Into::into)
    }

    pub(super) fn prepare_direct_call(
        module: &Module,
        caller_function: &Function,
        caller_activation: &Activation,
        arg_start: RegisterIndex,
        call: DirectCall,
    ) -> Result<Activation, InterpreterError> {
        let callee = module
            .function(call.callee())
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation = Activation::with_context(
            call.callee(),
            callee.frame_layout().register_count(),
            FrameMetadata::new(call.argument_count(), call.flags()),
            None,
        );
        let parameter_range = callee.frame_layout().parameter_range();
        let copy_count = call.argument_count().min(parameter_range.len());

        for offset in 0..copy_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            activation.set_register(parameter_range.start().saturating_add(offset), value)?;
        }

        Self::initialize_receiver(
            caller_function,
            caller_activation,
            callee,
            &mut activation,
            call.flags(),
            call.receiver(),
            None,
        )?;

        Ok(activation)
    }

    fn prepare_closure_call(
        caller_module: &Module,
        caller_activation: &Activation,
        runtime: &RuntimeState,
        callee_register: RegisterIndex,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<(Module, Activation), InterpreterError> {
        let closure = caller_activation
            .read_bytecode_register(
                caller_module
                    .function(caller_activation.function_index())
                    .expect("activation function index must be valid"),
                callee_register,
            )?
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(InterpreterError::InvalidObjectValue)?;
        let module = runtime.objects.closure_module(closure)?;
        let callee_index = runtime.objects.closure_callee(closure)?;
        let callee = module
            .function(callee_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation = Activation::with_context(
            callee_index,
            callee.frame_layout().register_count(),
            FrameMetadata::new(call.argument_count(), call.flags()),
            Some(closure),
        );
        let caller_function = caller_module
            .function(caller_activation.function_index())
            .expect("activation function index must be valid");
        let parameter_range = callee.frame_layout().parameter_range();
        let actual_argc = call.argument_count();
        let copy_count = actual_argc.min(parameter_range.len());

        for offset in 0..copy_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            activation.set_register(parameter_range.start().saturating_add(offset), value)?;
        }

        // ES2024 §10.4.4: Preserve overflow arguments for CreateArguments opcode.
        if actual_argc > parameter_range.len() {
            for offset in parameter_range.len()..actual_argc {
                let value = caller_activation
                    .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
                activation.overflow_args.push(value);
            }
        }

        Self::initialize_receiver(
            caller_function,
            caller_activation,
            callee,
            &mut activation,
            call.flags(),
            call.receiver(),
            None,
        )?;

        Ok((module, activation))
    }

    fn invoke_host_function(
        callable: ObjectHandle,
        caller_function: &Function,
        caller_activation: &Activation,
        runtime: &mut RuntimeState,
        host_function: HostFunctionId,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<Completion, InterpreterError> {
        let construct_receiver = if call.flags().is_construct() {
            if !Self::is_host_function_constructible(runtime, host_function)? {
                return Err(InterpreterError::InvalidCallTarget);
            }
            let intrinsic_default = Self::host_function_default_intrinsic(runtime, host_function);
            Some(RegisterValue::from_object_handle(
                Self::allocate_construct_receiver(runtime, callable, intrinsic_default)?.0,
            ))
        } else {
            None
        };
        let receiver = Self::resolve_call_receiver(
            caller_function,
            caller_activation,
            call.flags(),
            call.receiver(),
            construct_receiver,
        )?;
        let arguments = Self::read_call_arguments(
            caller_function,
            caller_activation,
            arg_start,
            call.argument_count(),
        )?;
        let completion = Self::invoke_registered_host_function(
            runtime,
            host_function,
            callable,
            receiver,
            &arguments,
            call.flags().is_construct(),
        )?;
        if let Some(default_receiver) = construct_receiver {
            Ok(Self::apply_construct_return_override(
                completion,
                default_receiver,
            ))
        } else {
            Ok(completion)
        }
    }

    fn read_call_arguments(
        caller_function: &Function,
        caller_activation: &Activation,
        arg_start: RegisterIndex,
        argument_count: RegisterIndex,
    ) -> Result<Vec<RegisterValue>, InterpreterError> {
        let mut arguments = Vec::with_capacity(usize::from(argument_count));
        for offset in 0..argument_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            arguments.push(value);
        }
        Ok(arguments)
    }

    fn resolve_call_receiver(
        caller_function: &Function,
        caller_activation: &Activation,
        flags: FrameFlags,
        receiver_register: Option<BytecodeRegister>,
        construct_receiver: Option<RegisterValue>,
    ) -> Result<RegisterValue, InterpreterError> {
        match receiver_register {
            Some(receiver_register) => {
                caller_activation.read_bytecode_register(caller_function, receiver_register.index())
            }
            None if flags.is_construct() => {
                Ok(construct_receiver.unwrap_or_else(RegisterValue::undefined))
            }
            None if flags.has_receiver() => Ok(RegisterValue::undefined()),
            None => Ok(RegisterValue::undefined()),
        }
    }

    pub(super) fn invoke_host_function_handle(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<Completion, InterpreterError> {
        // ES2024 §10.4.1.1 [[Call]] — resolve bound function chain.
        if let Ok(HeapValueKind::BoundFunction) = runtime.objects.kind(callable) {
            let (target, bound_this, bound_args) =
                runtime.objects.bound_function_parts(callable)?;
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return Self::invoke_host_function_handle(runtime, target, bound_this, &full_args);
        }

        // ES2024 §27.2.1.3 — Promise capability resolve/reject functions.
        if let Ok(HeapValueKind::PromiseCapabilityFunction) = runtime.objects.kind(callable) {
            let value = arguments
                .first()
                .copied()
                .unwrap_or(RegisterValue::undefined());
            Self::invoke_promise_capability_function(runtime, callable, value)?;
            return Ok(Completion::Return(RegisterValue::undefined()));
        }

        // Promise combinator per-element / finally / value-thunk dispatch.
        match runtime.objects.kind(callable) {
            Ok(HeapValueKind::PromiseCombinatorElement) => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                let result = Self::invoke_promise_combinator_element(runtime, callable, value)?;
                return Ok(Completion::Return(result));
            }
            Ok(HeapValueKind::PromiseFinallyFunction) => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                match Self::invoke_promise_finally_function(runtime, callable, value) {
                    Ok(v) => return Ok(Completion::Return(v)),
                    Err(InterpreterError::UncaughtThrow(v)) => {
                        return Ok(Completion::Throw(v));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(HeapValueKind::PromiseValueThunk) => {
                if let Some((v, k)) = runtime.objects.promise_value_thunk_info(callable) {
                    match k {
                        crate::promise::PromiseFinallyKind::ThenFinally => {
                            return Ok(Completion::Return(v));
                        }
                        crate::promise::PromiseFinallyKind::CatchFinally => {
                            return Ok(Completion::Throw(v));
                        }
                    }
                }
            }
            _ => {}
        }

        let host_function = runtime
            .objects
            .host_function(callable)?
            .ok_or(InterpreterError::InvalidCallTarget)?;
        Self::invoke_registered_host_function(
            runtime,
            host_function,
            callable,
            receiver,
            arguments,
            false,
        )
    }

    pub(super) fn invoke_registered_host_function(
        runtime: &mut RuntimeState,
        host_function: HostFunctionId,
        callee: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
        is_construct: bool,
    ) -> Result<Completion, InterpreterError> {
        let descriptor = runtime
            .native_functions()
            .get(host_function)
            .cloned()
            .ok_or(InterpreterError::InvalidCallTarget)?;

        runtime
            .check_interrupt()
            .map_err(|_| InterpreterError::Interrupted)?;

        // §9.4 Execution Contexts — the "running execution context" belongs
        // to the callee's realm for the duration of the call, so host
        // functions see `runtime.current_realm` = their own realm.
        let saved_realm = runtime.current_realm;
        runtime.current_realm = runtime.get_function_realm(callee);
        runtime.native_call_construct_stack.push(is_construct);
        runtime.native_callee_stack.push(callee);
        let completion = match (descriptor.callback())(&receiver, arguments, runtime) {
            Ok(value) => Ok(Completion::Return(value)),
            Err(VmNativeCallError::Thrown(value)) => Ok(Completion::Throw(value)),
            Err(VmNativeCallError::Internal(message))
                if runtime.is_execution_interrupted()
                    && message.as_ref() == EXECUTION_INTERRUPTED_MESSAGE =>
            {
                Err(InterpreterError::Interrupted)
            }
            Err(VmNativeCallError::Internal(message)) => Err(InterpreterError::NativeCall(message)),
        };
        runtime.native_callee_stack.pop();
        runtime.native_call_construct_stack.pop();
        runtime.current_realm = saved_realm;
        if completion.is_ok() && runtime.is_execution_interrupted() {
            return Err(InterpreterError::Interrupted);
        }
        completion
    }

    fn initialize_receiver(
        caller_function: &Function,
        caller_activation: &Activation,
        callee_function: &Function,
        callee_activation: &mut Activation,
        flags: FrameFlags,
        receiver_register: Option<BytecodeRegister>,
        construct_receiver: Option<RegisterValue>,
    ) -> Result<(), InterpreterError> {
        let receiver = match receiver_register {
            Some(receiver_register) => caller_activation
                .read_bytecode_register(caller_function, receiver_register.index())?,
            None if flags.is_construct() => {
                construct_receiver.unwrap_or_else(RegisterValue::undefined)
            }
            None if flags.has_receiver()
                || callee_function.frame_layout().receiver_slot().is_some() =>
            {
                RegisterValue::undefined()
            }
            None => return Ok(()),
        };

        if callee_function.frame_layout().receiver_slot().is_some() {
            callee_activation.set_receiver(callee_function, receiver)?;
        }

        Ok(())
    }

    /// §10.1.13 OrdinaryCreateFromConstructor: allocate a fresh ordinary
    /// object whose [[Prototype]] is taken from `constructor.prototype`, or
    /// from the constructor's realm's `intrinsic_default` when that property
    /// is not an object.
    pub(super) fn allocate_construct_receiver(
        runtime: &mut RuntimeState,
        constructor: ObjectHandle,
        intrinsic_default: crate::intrinsics::IntrinsicKey,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype = runtime.get_prototype_from_constructor(constructor, intrinsic_default)?;
        Ok(runtime.alloc_object_with_prototype(Some(prototype)))
    }

    /// Returns the `IntrinsicKey` that the given host function's descriptor
    /// declares as its `intrinsicDefaultProto` (§10.1.14), if any. Falls back
    /// to `ObjectPrototype`.
    pub(super) fn host_function_default_intrinsic(
        runtime: &RuntimeState,
        host_function: HostFunctionId,
    ) -> crate::intrinsics::IntrinsicKey {
        runtime
            .native_functions()
            .get(host_function)
            .and_then(NativeFunctionDescriptor::default_intrinsic)
            .unwrap_or(crate::intrinsics::IntrinsicKey::ObjectPrototype)
    }

    fn is_host_function_constructible(
        runtime: &RuntimeState,
        host_function: HostFunctionId,
    ) -> Result<bool, InterpreterError> {
        let descriptor = runtime
            .native_functions()
            .get(host_function)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        Ok(descriptor.slot_kind() == NativeSlotKind::Constructor)
    }

    pub(super) fn apply_construct_return_override(
        completion: Completion,
        default_receiver: RegisterValue,
    ) -> Completion {
        match completion {
            Completion::Return(value) if value.as_object_handle().is_some() => {
                Completion::Return(value)
            }
            Completion::Return(_) => Completion::Return(default_receiver),
            Completion::Throw(value) => Completion::Throw(value),
        }
    }

    /// §14.4.4 yield* delegation forwarding result.
    fn handle_yield_star_delegation(
        runtime: &mut RuntimeState,
        _generator: ObjectHandle,
        inner_iter: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: crate::intrinsics::GeneratorResumeKind,
        _resume_reg: u16,
    ) -> Result<YieldStarResult, VmNativeCallError> {
        use crate::intrinsics::GeneratorResumeKind;

        fn interp_to_native(e: InterpreterError) -> VmNativeCallError {
            match e {
                InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                InterpreterError::TypeError(m) | InterpreterError::NativeCall(m) => {
                    VmNativeCallError::Internal(m)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            }
        }

        match resume_kind {
            GeneratorResumeKind::Next => {
                let (done, value) = runtime
                    .call_iterator_next_with_value(inner_iter, sent_value)
                    .map_err(interp_to_native)?;
                if done {
                    Ok(YieldStarResult::Done(value))
                } else {
                    Ok(YieldStarResult::Yield(value))
                }
            }
            GeneratorResumeKind::Throw => {
                // §14.4.4 step 7.b — forward .throw() to inner iterator.
                match runtime
                    .call_iterator_throw(inner_iter, sent_value)
                    .map_err(interp_to_native)?
                {
                    Some((done, value)) => {
                        if done {
                            Ok(YieldStarResult::Done(value))
                        } else {
                            Ok(YieldStarResult::Yield(value))
                        }
                    }
                    None => {
                        // Inner iterator has no .throw() — close it and throw TypeError.
                        let _ = runtime.objects.iterator_close(inner_iter);
                        let err = runtime
                            .alloc_type_error("The iterator does not provide a 'throw' method")
                            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                        Err(VmNativeCallError::Thrown(
                            RegisterValue::from_object_handle(err.0),
                        ))
                    }
                }
            }
            GeneratorResumeKind::Return => {
                // §14.4.4 step 7.c — forward .return() to inner iterator.
                match runtime
                    .call_iterator_return(inner_iter, sent_value)
                    .map_err(interp_to_native)?
                {
                    Some((done, value)) => {
                        if done {
                            Ok(YieldStarResult::Return(value))
                        } else {
                            Ok(YieldStarResult::Yield(value))
                        }
                    }
                    None => {
                        // Inner iterator has no .return() — just return the value.
                        Ok(YieldStarResult::Return(sent_value))
                    }
                }
            }
        }
    }

    /// Core generator resume implementation.
    ///
    /// Called from `RuntimeState::resume_generator` to execute generator body
    /// until the next yield, return, or throw.
    pub(super) fn resume_generator_impl(
        runtime: &mut RuntimeState,
        generator: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: crate::intrinsics::GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        use crate::intrinsics::GeneratorResumeKind;
        use crate::object::GeneratorState;

        let (
            module,
            function_index,
            closure_handle,
            arguments,
            saved_registers,
            resume_pc,
            resume_reg,
        ) = runtime
            .objects
            .generator_take_state(generator)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("generator take state: {e:?}").into())
            })?;

        let function = module.function(function_index).ok_or_else(|| {
            VmNativeCallError::Internal("generator function index invalid".into())
        })?;

        let register_count = function.frame_layout().register_count();
        let had_saved_registers = saved_registers.is_some();

        // Build the activation.
        let mut activation = if let Some(saved_regs) = saved_registers {
            // Resuming from a yield point — restore the saved registers.
            let mut act = Activation::with_context(
                function_index,
                register_count,
                FrameMetadata::default(),
                closure_handle,
            );
            act.restore_registers(&saved_regs);
            act.set_pc(resume_pc);

            match resume_kind {
                GeneratorResumeKind::Next => {
                    act.write_bytecode_register(function, resume_reg, sent_value)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator resume write: {e:?}").into(),
                            )
                        })?;
                }
                GeneratorResumeKind::Return => {
                    // For .return() on a yielded generator, mark completed.
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator set state: {e:?}").into(),
                            )
                        })?;
                    let result = runtime.create_iter_result(sent_value, true)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
                GeneratorResumeKind::Throw => {
                    // We will inject the throw at the first step.
                }
            }
            act
        } else {
            // SuspendedStart — first call to .next().
            // Set up arguments in the activation's parameter registers.
            match resume_kind {
                GeneratorResumeKind::Next => {
                    let mut act = Activation::with_context(
                        function_index,
                        register_count,
                        FrameMetadata::new(arguments.len() as u16, FrameFlags::empty()),
                        closure_handle,
                    );
                    // Write arguments to parameter registers.
                    let param_count = function.frame_layout().parameter_count();
                    for (i, &arg) in arguments.iter().enumerate() {
                        if i >= param_count as usize {
                            break;
                        }
                        let _ = act.write_bytecode_register(function, i as u16, arg);
                    }
                    act
                }
                GeneratorResumeKind::Return => {
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator set state: {e:?}").into(),
                            )
                        })?;
                    let result = runtime.create_iter_result(sent_value, true)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
                GeneratorResumeKind::Throw => {
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator set state: {e:?}").into(),
                            )
                        })?;
                    return Err(VmNativeCallError::Thrown(sent_value));
                }
            }
        };

        let interp = Interpreter::for_runtime(runtime);
        let previous_module = runtime.enter_module(&module);

        // §14.4.4 — Check for active yield* delegation before entering the execution loop.
        // If a delegation iterator is active, forward the resume to it.
        // NOTE: enter_module must be called BEFORE this block so that
        // call_callable_for_accessor can dispatch through Interpreter::call_function
        // (otherwise current_module is None and it falls back to call_host_function).
        if had_saved_registers {
            let delegation = runtime
                .objects
                .generator_delegation_iterator(generator)
                .unwrap_or(None);
            if let Some(inner_iter) = delegation {
                match Self::handle_yield_star_delegation(
                    runtime,
                    generator,
                    inner_iter,
                    sent_value,
                    resume_kind,
                    resume_reg,
                ) {
                    Ok(YieldStarResult::Yield(yielded_value)) => {
                        // Inner iterator not done — save state and yield the inner value.
                        let saved_regs = activation.save_registers();
                        let pc = activation.pc();
                        runtime
                            .objects
                            .generator_save_state(generator, saved_regs, pc, resume_reg)
                            .map_err(|e| {
                                VmNativeCallError::Internal(
                                    format!("generator save state: {e:?}").into(),
                                )
                            })?;
                        runtime.restore_module(previous_module);
                        let result = runtime.create_iter_result(yielded_value, false)?;
                        return Ok(RegisterValue::from_object_handle(result.0));
                    }
                    Ok(YieldStarResult::Done(return_value)) => {
                        // Inner iterator done — clear delegation, write return value
                        // to the resume register, and continue generator execution.
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, None);
                        activation
                            .write_bytecode_register(function, resume_reg, return_value)
                            .map_err(|e| {
                                VmNativeCallError::Internal(
                                    format!("generator delegation write: {e:?}").into(),
                                )
                            })?;
                        // Fall through to the normal execution loop below.
                    }
                    Ok(YieldStarResult::Return(return_value)) => {
                        // .return() propagated from inner — complete the generator.
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, None);
                        runtime
                            .objects
                            .set_generator_state(generator, GeneratorState::Completed)
                            .ok();
                        runtime.restore_module(previous_module);
                        let result = runtime.create_iter_result(return_value, true)?;
                        return Ok(RegisterValue::from_object_handle(result.0));
                    }
                    Err(VmNativeCallError::Thrown(thrown)) => {
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, None);
                        runtime
                            .objects
                            .set_generator_state(generator, GeneratorState::Completed)
                            .ok();
                        runtime.restore_module(previous_module);
                        return Err(VmNativeCallError::Thrown(thrown));
                    }
                    Err(e) => {
                        runtime.restore_module(previous_module);
                        return Err(e);
                    }
                }
            }
        }
        let mut frame_runtime = FrameRuntimeState::new(function);

        // For Throw resume kind on a yielded generator, inject exception.
        let mut inject_throw =
            matches!(resume_kind, GeneratorResumeKind::Throw) && had_saved_registers;

        loop {
            activation.begin_step();

            if inject_throw {
                inject_throw = false;
                // The saved resume PC is past the Yield instruction (Yield
                // advances before saving state). Back up by 1 so that
                // transfer_exception sees the PC at the Yield, which is
                // inside any enclosing try/catch handler range.
                let current_pc = activation.pc();
                if current_pc > 0 {
                    activation.set_pc(current_pc - 1);
                }
                if interp.transfer_exception(function, &mut activation, sent_value) {
                    continue;
                }
                runtime.restore_module(previous_module);
                runtime
                    .objects
                    .set_generator_state(generator, GeneratorState::Completed)
                    .ok();
                return Err(VmNativeCallError::Thrown(sent_value));
            }

            let outcome = match interp.step(
                function,
                &module,
                &mut activation,
                runtime,
                &mut frame_runtime,
            ) {
                Ok(outcome) => outcome,
                Err(InterpreterError::UncaughtThrow(value)) => StepOutcome::Throw(value),
                Err(InterpreterError::TypeError(message)) => {
                    let error = runtime
                        .alloc_type_error(&message)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                }
                Err(error) => {
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Internal(
                        format!("generator execution error: {error:?}").into(),
                    ));
                }
            };

            match outcome {
                StepOutcome::Continue => {
                    activation
                        .sync_written_open_upvalues(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    activation
                        .refresh_open_upvalues_from_cells(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                }
                StepOutcome::Return(return_value) => {
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    let result = runtime.create_iter_result(return_value, true)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
                StepOutcome::Throw(value) => {
                    if interp.transfer_exception(function, &mut activation, value) {
                        continue;
                    }
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Thrown(value));
                }
                // TailCallClosure is never emitted for generators (compiler
                // skips TCO for generator/async function kinds).
                StepOutcome::TailCall { .. } => {
                    unreachable!("TailCallClosure inside generator body")
                }
                StepOutcome::Suspend { .. } => {
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Internal(
                        "await inside generator not yet supported".into(),
                    ));
                }
                StepOutcome::GeneratorYield {
                    yielded_value,
                    resume_register: yield_resume_reg,
                } => {
                    let saved_regs = activation.save_registers();
                    let pc = activation.pc();
                    runtime
                        .objects
                        .generator_save_state(generator, saved_regs, pc, yield_resume_reg)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator save state: {e:?}").into(),
                            )
                        })?;
                    // §14.4.4 — if YieldStar set a pending delegation, store it.
                    if let Some(inner_iter) = runtime.pending_delegation_iterator.take() {
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, Some(inner_iter));
                    }
                    runtime.restore_module(previous_module);
                    let result = runtime.create_iter_result(yielded_value, false)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
            }
        }
    }

    /// §15.8.3 — Eagerly execute async generator param init (FDI).
    ///
    /// Runs the async generator from PC 0 until the implicit initial Yield
    /// (emitted after parameter initialization). If param init throws, the
    /// error propagates to the caller. Otherwise the generator transitions
    /// from SuspendedStart to SuspendedYield with params initialized.
    fn run_async_generator_param_init(
        runtime: &mut RuntimeState,
        generator: ObjectHandle,
    ) -> Result<(), VmNativeCallError> {
        use crate::object::GeneratorState;

        let (module, function_index, closure_handle, arguments, _saved, _pc, _resume) = runtime
            .objects
            .async_generator_take_state(generator)
            .map_err(|e| {
                VmNativeCallError::Internal(
                    format!("async gen param init take state: {e:?}").into(),
                )
            })?;

        let function = module
            .function(function_index)
            .ok_or_else(|| {
                VmNativeCallError::Internal("async gen param init: missing function".into())
            })?
            .clone();

        let register_count = function.frame_layout().register_count();
        let mut act = Activation::with_context(
            function_index,
            register_count,
            FrameMetadata::new(arguments.len() as u16, FrameFlags::empty()),
            closure_handle,
        );
        let param_count = function.frame_layout().parameter_count();
        for (i, &arg) in arguments.iter().enumerate() {
            if i >= param_count as usize {
                break;
            }
            let _ = act.write_bytecode_register(&function, i as u16, arg);
        }

        let interp = Interpreter::for_runtime(runtime);
        let previous_module = runtime.enter_module(&module);
        let mut frame_runtime = FrameRuntimeState::new(&function);

        loop {
            act.begin_step();
            let outcome =
                match interp.step(&function, &module, &mut act, runtime, &mut frame_runtime) {
                    Ok(o) => o,
                    Err(InterpreterError::UncaughtThrow(v)) => StepOutcome::Throw(v),
                    Err(InterpreterError::TypeError(message)) => {
                        let error = runtime
                            .alloc_type_error(&message)
                            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                        StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                    }
                    Err(e) => {
                        runtime.restore_module(previous_module);
                        runtime
                            .objects
                            .set_async_generator_state(generator, GeneratorState::Completed)
                            .ok();
                        return Err(VmNativeCallError::Internal(
                            format!("async gen param init error: {e:?}").into(),
                        ));
                    }
                };

            match outcome {
                StepOutcome::Continue => {
                    act.sync_written_open_upvalues(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    act.refresh_open_upvalues_from_cells(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                }
                StepOutcome::GeneratorYield {
                    resume_register, ..
                } => {
                    // Hit the implicit initial yield — save state.
                    let saved_regs = act.save_registers();
                    let pc = act.pc();
                    runtime
                        .objects
                        .async_generator_save_state(generator, saved_regs, pc, resume_register)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("async gen save state: {e:?}").into(),
                            )
                        })?;
                    runtime.restore_module(previous_module);
                    return Ok(());
                }
                StepOutcome::Throw(value) => {
                    if interp.transfer_exception(&function, &mut act, value) {
                        continue;
                    }
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Thrown(value));
                }
                StepOutcome::Return(_) => {
                    // Generator completed during param init (unlikely but handle it)
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Ok(());
                }
                _ => {
                    runtime.restore_module(previous_module);
                    return Err(VmNativeCallError::Internal(
                        "unexpected step outcome in async gen param init".into(),
                    ));
                }
            }
        }
    }

    /// Core async generator resume implementation.
    ///
    /// §27.6.3.3 AsyncGeneratorResume
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorresume>
    ///
    /// Peeks at the front request, resumes the body. On yield, saves state
    /// and settles the front request's promise with `{value, done: false}`.
    /// On return/throw, marks completed and drains the queue.
    pub(super) fn resume_async_generator_impl(
        runtime: &mut RuntimeState,
        generator: ObjectHandle,
    ) -> Result<(), VmNativeCallError> {
        use crate::intrinsics::async_generator_class::{
            async_generator_complete_step, async_generator_drain_completed,
        };
        use crate::object::{AsyncGeneratorRequestKind, GeneratorState};

        // Peek the front request to determine resume kind + value.
        let request = runtime
            .objects
            .async_generator_peek_request(generator)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("async generator peek request: {e:?}").into())
            })?;
        let Some(request) = request else {
            // No pending requests — nothing to do.
            return Ok(());
        };

        let resume_kind = request.kind;
        let sent_value = request.value;

        // Take state from the async generator (transitions to Executing).
        let (
            module,
            function_index,
            closure_handle,
            arguments,
            saved_registers,
            resume_pc,
            resume_reg,
        ) = runtime
            .objects
            .async_generator_take_state(generator)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("async generator take state: {e:?}").into())
            })?;

        let function = module.function(function_index).ok_or_else(|| {
            VmNativeCallError::Internal("async generator function index invalid".into())
        })?;

        let register_count = function.frame_layout().register_count();
        let had_saved_registers = saved_registers.is_some();

        let mut activation = if let Some(saved_regs) = saved_registers {
            let mut act = Activation::with_context(
                function_index,
                register_count,
                FrameMetadata::default(),
                closure_handle,
            );
            act.restore_registers(&saved_regs);
            act.set_pc(resume_pc);

            match resume_kind {
                AsyncGeneratorRequestKind::Next => {
                    act.write_bytecode_register(function, resume_reg, sent_value)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("async gen resume write: {e:?}").into(),
                            )
                        })?;
                }
                AsyncGeneratorRequestKind::Return => {
                    // §27.6.3.5 AsyncGeneratorAwaitReturn — complete the request,
                    // mark completed, and drain.
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    let _ = runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed);
                    async_generator_complete_step(runtime, request.promise, sent_value, true)?;
                    async_generator_drain_completed(generator, runtime)?;
                    return Ok(());
                }
                AsyncGeneratorRequestKind::Throw => {
                    // Will inject throw at first step.
                }
            }
            act
        } else {
            // SuspendedStart — first call to .next().
            match resume_kind {
                AsyncGeneratorRequestKind::Next => {
                    let mut act = Activation::with_context(
                        function_index,
                        register_count,
                        FrameMetadata::new(arguments.len() as u16, FrameFlags::empty()),
                        closure_handle,
                    );
                    let param_count = function.frame_layout().parameter_count();
                    for (i, &arg) in arguments.iter().enumerate() {
                        if i >= param_count as usize {
                            break;
                        }
                        let _ = act.write_bytecode_register(function, i as u16, arg);
                    }
                    act
                }
                AsyncGeneratorRequestKind::Return => {
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    let _ = runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed);
                    async_generator_complete_step(runtime, request.promise, sent_value, true)?;
                    async_generator_drain_completed(generator, runtime)?;
                    return Ok(());
                }
                AsyncGeneratorRequestKind::Throw => {
                    // §27.6.1.4 step 10: If state is suspendedStart, reject.
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    let _ = runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed);
                    // Reject the promise with the thrown value.
                    if let Some(p) = runtime.objects.get_promise_mut(request.promise)
                        && p.is_pending()
                        && let Some(jobs) = p.reject(sent_value)
                    {
                        for job in jobs {
                            runtime.microtasks_mut().enqueue_promise_job(job);
                        }
                    }
                    async_generator_drain_completed(generator, runtime)?;
                    return Ok(());
                }
            }
        };

        let interp = Interpreter::for_runtime(runtime);
        let previous_module = runtime.enter_module(&module);

        // §14.4.4 — Check for active yield* delegation before entering the execution loop.
        if had_saved_registers {
            let delegation = runtime
                .objects
                .generator_delegation_iterator(generator)
                .unwrap_or(None);
            if let Some(inner_iter) = delegation {
                // Convert async generator request kind to sync GeneratorResumeKind
                // for the delegation handler.
                let gen_resume_kind = match resume_kind {
                    AsyncGeneratorRequestKind::Next => crate::intrinsics::GeneratorResumeKind::Next,
                    AsyncGeneratorRequestKind::Return => {
                        crate::intrinsics::GeneratorResumeKind::Return
                    }
                    AsyncGeneratorRequestKind::Throw => {
                        crate::intrinsics::GeneratorResumeKind::Throw
                    }
                };
                match Self::handle_yield_star_delegation(
                    runtime,
                    generator,
                    inner_iter,
                    sent_value,
                    gen_resume_kind,
                    resume_reg,
                ) {
                    Ok(YieldStarResult::Yield(yielded_value)) => {
                        // Inner iterator not done — save state and yield the inner value.
                        let saved_regs = activation.save_registers();
                        let pc = activation.pc();
                        runtime
                            .objects
                            .async_generator_save_state(generator, saved_regs, pc, resume_reg)
                            .map_err(|e| {
                                VmNativeCallError::Internal(
                                    format!("async gen save state: {e:?}").into(),
                                )
                            })?;
                        runtime.restore_module(previous_module);
                        // Dequeue front request and resolve with {value, done: false}.
                        let _ = runtime.objects.async_generator_dequeue(generator);
                        async_generator_complete_step(
                            runtime,
                            request.promise,
                            yielded_value,
                            false,
                        )?;
                        // If more queued requests, resume immediately.
                        let queue_empty = runtime
                            .objects
                            .async_generator_queue_is_empty(generator)
                            .unwrap_or(true);
                        if !queue_empty {
                            return Self::resume_async_generator_impl(runtime, generator);
                        }
                        return Ok(());
                    }
                    Ok(YieldStarResult::Done(return_value)) => {
                        // Inner iterator done — clear delegation, write return value
                        // to the resume register, and continue execution.
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, None);
                        activation
                            .write_bytecode_register(function, resume_reg, return_value)
                            .map_err(|e| {
                                VmNativeCallError::Internal(
                                    format!("async gen delegation write: {e:?}").into(),
                                )
                            })?;
                        // Fall through to the normal execution loop below.
                    }
                    Ok(YieldStarResult::Return(return_value)) => {
                        // .return() propagated from inner — complete the async generator.
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, None);
                        let _ = runtime
                            .objects
                            .set_async_generator_state(generator, GeneratorState::Completed);
                        runtime.restore_module(previous_module);
                        let _ = runtime.objects.async_generator_dequeue(generator);
                        async_generator_complete_step(
                            runtime,
                            request.promise,
                            return_value,
                            true,
                        )?;
                        async_generator_drain_completed(generator, runtime)?;
                        return Ok(());
                    }
                    Err(VmNativeCallError::Thrown(thrown)) => {
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, None);
                        let _ = runtime
                            .objects
                            .set_async_generator_state(generator, GeneratorState::Completed);
                        runtime.restore_module(previous_module);
                        let _ = runtime.objects.async_generator_dequeue(generator);
                        if let Some(p) = runtime.objects.get_promise_mut(request.promise)
                            && p.is_pending()
                            && let Some(jobs) = p.reject(thrown)
                        {
                            for job in jobs {
                                runtime.microtasks_mut().enqueue_promise_job(job);
                            }
                        }
                        async_generator_drain_completed(generator, runtime)?;
                        return Ok(());
                    }
                    Err(e) => {
                        runtime.restore_module(previous_module);
                        return Err(e);
                    }
                }
            }
        }

        let mut frame_runtime = FrameRuntimeState::new(function);

        let mut inject_throw =
            matches!(resume_kind, AsyncGeneratorRequestKind::Throw) && had_saved_registers;

        loop {
            activation.begin_step();

            if inject_throw {
                inject_throw = false;
                // The saved resume PC is past the Yield instruction (Yield
                // advances before saving state). Back up by 1 so that
                // transfer_exception sees the PC at the Yield, which is
                // inside any enclosing try/catch handler range.
                let current_pc = activation.pc();
                if current_pc > 0 {
                    activation.set_pc(current_pc - 1);
                }
                if interp.transfer_exception(function, &mut activation, sent_value) {
                    continue;
                }
                // Exception not caught — complete with rejection.
                runtime.restore_module(previous_module);
                let _ = runtime
                    .objects
                    .set_async_generator_state(generator, GeneratorState::Completed);
                let _ = runtime.objects.async_generator_dequeue(generator);
                if let Some(p) = runtime.objects.get_promise_mut(request.promise)
                    && p.is_pending()
                    && let Some(jobs) = p.reject(sent_value)
                {
                    for job in jobs {
                        runtime.microtasks_mut().enqueue_promise_job(job);
                    }
                }
                async_generator_drain_completed(generator, runtime)?;
                return Ok(());
            }

            let outcome = match interp.step(
                function,
                &module,
                &mut activation,
                runtime,
                &mut frame_runtime,
            ) {
                Ok(outcome) => outcome,
                Err(InterpreterError::UncaughtThrow(value)) => StepOutcome::Throw(value),
                Err(InterpreterError::TypeError(message)) => {
                    let error = runtime
                        .alloc_type_error(&message)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                }
                Err(error) => {
                    runtime.restore_module(previous_module);
                    let _ = runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed);
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    if let Some(p) = runtime.objects.get_promise_mut(request.promise)
                        && p.is_pending()
                        && let Some(jobs) = p.reject(RegisterValue::undefined())
                    {
                        for job in jobs {
                            runtime.microtasks_mut().enqueue_promise_job(job);
                        }
                    }
                    return Err(VmNativeCallError::Internal(
                        format!("async generator execution error: {error:?}").into(),
                    ));
                }
            };

            match outcome {
                StepOutcome::Continue => {
                    activation
                        .sync_written_open_upvalues(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    activation
                        .refresh_open_upvalues_from_cells(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                }
                StepOutcome::Return(return_value) => {
                    // §27.6.3.7 AsyncGeneratorCompleteStep — resolve with {value, done:true}.
                    runtime.restore_module(previous_module);
                    let _ = runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed);
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    async_generator_complete_step(runtime, request.promise, return_value, true)?;
                    // Drain remaining requests since generator is now completed.
                    async_generator_drain_completed(generator, runtime)?;
                    return Ok(());
                }
                StepOutcome::Throw(value) => {
                    if interp.transfer_exception(function, &mut activation, value) {
                        continue;
                    }
                    // Uncaught — reject the front request's promise.
                    runtime.restore_module(previous_module);
                    let _ = runtime
                        .objects
                        .set_async_generator_state(generator, GeneratorState::Completed);
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    if let Some(p) = runtime.objects.get_promise_mut(request.promise)
                        && p.is_pending()
                        && let Some(jobs) = p.reject(value)
                    {
                        for job in jobs {
                            runtime.microtasks_mut().enqueue_promise_job(job);
                        }
                    }
                    async_generator_drain_completed(generator, runtime)?;
                    return Ok(());
                }
                StepOutcome::Suspend {
                    awaited_promise,
                    resume_register: await_resume_reg,
                } => {
                    // Await inside async generator — synchronously poll the
                    // awaited promise (same approach as async functions in our
                    // single-threaded event loop model).
                    if let Some(promise) = runtime.objects.get_promise(awaited_promise) {
                        match &promise.state {
                            crate::promise::PromiseState::Fulfilled(result) => {
                                let result = *result;
                                activation
                                    .set_register(await_resume_reg, result)
                                    .map_err(|e| {
                                        VmNativeCallError::Internal(format!("{e:?}").into())
                                    })?;
                                continue;
                            }
                            crate::promise::PromiseState::Rejected(reason) => {
                                let reason = *reason;
                                // Back up PC past the Await advance so
                                // transfer_exception finds the try/catch.
                                let current_pc = activation.pc();
                                if current_pc > 0 {
                                    activation.set_pc(current_pc - 1);
                                }
                                if interp.transfer_exception(function, &mut activation, reason) {
                                    continue;
                                }
                                // Uncaught — reject the front request.
                                runtime.restore_module(previous_module);
                                let _ = runtime.objects.set_async_generator_state(
                                    generator,
                                    GeneratorState::Completed,
                                );
                                let _ = runtime.objects.async_generator_dequeue(generator);
                                if let Some(p) = runtime.objects.get_promise_mut(request.promise)
                                    && p.is_pending()
                                    && let Some(jobs) = p.reject(reason)
                                {
                                    for job in jobs {
                                        runtime.microtasks_mut().enqueue_promise_job(job);
                                    }
                                }
                                async_generator_drain_completed(generator, runtime)?;
                                return Ok(());
                            }
                            crate::promise::PromiseState::Pending => {
                                // Save state, suspend. The promise handler
                                // will need to resume later.
                                let saved_regs = activation.save_registers();
                                let pc = activation.pc();
                                runtime
                                    .objects
                                    .async_generator_save_state(
                                        generator,
                                        saved_regs,
                                        pc,
                                        await_resume_reg,
                                    )
                                    .map_err(|e| {
                                        VmNativeCallError::Internal(format!("{e:?}").into())
                                    })?;
                                runtime.restore_module(previous_module);
                                // TODO: Register promise reaction to resume.
                                return Ok(());
                            }
                        }
                    }
                    // Not a promise — treat as immediately resolved.
                    let await_val = RegisterValue::from_object_handle(awaited_promise.0);
                    activation
                        .set_register(await_resume_reg, await_val)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    continue;
                }
                // TailCallClosure is never emitted for async generators.
                StepOutcome::TailCall { .. } => {
                    unreachable!("TailCallClosure inside async generator body")
                }
                StepOutcome::GeneratorYield {
                    yielded_value,
                    resume_register: yield_resume_reg,
                } => {
                    // §27.6.3.8 AsyncGeneratorYield — save state, settle front
                    // request with {value, done: false}, leave queued requests.
                    let saved_regs = activation.save_registers();
                    let pc = activation.pc();
                    runtime
                        .objects
                        .async_generator_save_state(generator, saved_regs, pc, yield_resume_reg)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("async gen save state: {e:?}").into(),
                            )
                        })?;
                    // §14.4.4 — if YieldStar set a pending delegation, store it.
                    if let Some(inner_iter) = runtime.pending_delegation_iterator.take() {
                        let _ = runtime
                            .objects
                            .set_generator_delegation_iterator(generator, Some(inner_iter));
                    }
                    runtime.restore_module(previous_module);

                    // Dequeue the front request and resolve its promise.
                    let _ = runtime.objects.async_generator_dequeue(generator);
                    async_generator_complete_step(runtime, request.promise, yielded_value, false)?;

                    // If there are more queued requests, resume immediately.
                    let queue_empty = runtime
                        .objects
                        .async_generator_queue_is_empty(generator)
                        .unwrap_or(true);
                    if !queue_empty {
                        return Self::resume_async_generator_impl(runtime, generator);
                    }

                    return Ok(());
                }
            }
        }
    }
}

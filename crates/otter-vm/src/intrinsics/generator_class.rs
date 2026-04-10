//! Generator function and prototype intrinsics.
//!
//! Spec:
//! - %GeneratorFunction%: <https://tc39.es/ecma262/#sec-generatorfunction-objects>
//! - %GeneratorFunction.prototype%: <https://tc39.es/ecma262/#sec-properties-of-the-generatorfunction-prototype-object>
//! - %GeneratorPrototype%: <https://tc39.es/ecma262/#sec-properties-of-the-generator-prototype>

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{
    GeneratorState, HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue,
};
use crate::value::RegisterValue;

use super::install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name};
use super::{IntrinsicsError, VmIntrinsics, WellKnownSymbol};

pub(super) static GENERATOR_INTRINSIC: GeneratorIntrinsic = GeneratorIntrinsic;

pub(super) struct GeneratorIntrinsic;

impl IntrinsicInstaller for GeneratorIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // ─── §27.3.3 %GeneratorFunction.prototype% ────────────────────
        // %GeneratorFunction.prototype%.prototype = %GeneratorPrototype%
        let gen_proto = intrinsics.generator_prototype();
        let gen_fn_proto = intrinsics.generator_function_prototype();

        let prototype_prop = cx.property_names.intern("prototype");
        cx.heap.define_own_property(
            gen_fn_proto,
            prototype_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(gen_proto.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // %GeneratorFunction.prototype%[@@toStringTag] = "GeneratorFunction"
        install_to_string_tag(gen_fn_proto, "GeneratorFunction", cx)?;

        // §25.2.1 The GeneratorFunction Constructor
        let constructor_prop = cx.property_names.intern("constructor");
        let gen_fn_ctor_desc = NativeFunctionDescriptor::constructor(
            "GeneratorFunction",
            1,
            generator_function_constructor,
        );
        let gen_fn_ctor_id = cx.native_functions.register(gen_fn_ctor_desc);
        let gen_fn_ctor =
            cx.alloc_intrinsic_host_function(gen_fn_ctor_id, intrinsics.function_prototype())?;
        install_function_length_name(gen_fn_ctor, 1, "GeneratorFunction", cx)?;
        // Constructor.prototype = %GeneratorFunction.prototype%
        cx.heap.define_own_property(
            gen_fn_ctor,
            prototype_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(gen_fn_proto.0),
                PropertyAttributes::from_flags(false, false, false),
            ),
        )?;
        // %GeneratorFunction.prototype%.constructor = GeneratorFunction
        cx.heap.define_own_property(
            gen_fn_proto,
            constructor_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(gen_fn_ctor.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // ─── §27.5.1 %GeneratorPrototype% ─────────────────────────────
        // %GeneratorPrototype%.constructor = %GeneratorFunction.prototype%
        let constructor_prop = cx.property_names.intern("constructor");
        cx.heap.define_own_property(
            gen_proto,
            constructor_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(gen_fn_proto.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // %GeneratorPrototype%.next(value)
        install_method(
            gen_proto,
            "next",
            1,
            generator_prototype_next,
            intrinsics.function_prototype(),
            cx,
        )?;

        // %GeneratorPrototype%.return(value)
        install_method(
            gen_proto,
            "return",
            1,
            generator_prototype_return,
            intrinsics.function_prototype(),
            cx,
        )?;

        // %GeneratorPrototype%.throw(value)
        install_method(
            gen_proto,
            "throw",
            1,
            generator_prototype_throw,
            intrinsics.function_prototype(),
            cx,
        )?;

        // %GeneratorPrototype%[@@toStringTag] = "Generator"
        install_to_string_tag(gen_proto, "Generator", cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        _intrinsics: &VmIntrinsics,
        _cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Generator prototypes are not directly exposed as globals.
        // %GeneratorFunction% is accessed via `Object.getPrototypeOf(function*(){})`.
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Native implementations
// ═══════════════════════════════════════════════════════════════════════════

type NativeFn = fn(
    &RegisterValue,
    &[RegisterValue],
    &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>;

/// §25.2.1.1 GeneratorFunction(p1, p2, ..., pn, body)
/// Same as Function constructor but wraps source as `function*`.
/// Spec: <https://tc39.es/ecma262/#sec-generatorfunction>
fn generator_function_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let (params, body) = if args.is_empty() {
        (String::new(), String::new())
    } else if args.len() == 1 {
        let body_str = runtime
            .js_to_string(args[0])
            .map_err(|e| VmNativeCallError::Internal(format!("GeneratorFunction: {e}").into()))?;
        (String::new(), body_str.to_string())
    } else {
        let mut param_parts = Vec::with_capacity(args.len() - 1);
        for arg in &args[..args.len() - 1] {
            let s = runtime.js_to_string(*arg).map_err(|e| {
                VmNativeCallError::Internal(format!("GeneratorFunction: {e}").into())
            })?;
            param_parts.push(s.to_string());
        }
        let body_str = runtime
            .js_to_string(args[args.len() - 1])
            .map_err(|e| VmNativeCallError::Internal(format!("GeneratorFunction: {e}").into()))?;
        (param_parts.join(","), body_str.to_string())
    };

    let source = format!("(function* anonymous({params}) {{\n{body}\n}})");
    let result = runtime.eval_source(&source, false, false)?;

    // §10.1.13 OrdinaryCreateFromConstructor — honour newTarget.prototype
    if let Some(handle) = result.as_object_handle().map(ObjectHandle) {
        let default_proto = runtime.intrinsics().generator_function_prototype();
        let target = runtime.subclass_prototype_or_default(*this, default_proto);
        if target != default_proto {
            runtime
                .objects_mut()
                .set_prototype(handle, Some(target))
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("GeneratorFunction subclass prototype install failed: {error:?}")
                            .into(),
                    )
                })?;
        }
    }

    Ok(result)
}

/// ES2024 §27.5.1.2 %GeneratorPrototype%.next(value)
/// Spec: <https://tc39.es/ecma262/#sec-generator.prototype.next>
fn generator_prototype_next(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let generator = require_generator_object(*this, runtime)?;
    let sent_value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let state = runtime
        .objects()
        .generator_state(generator)
        .map_err(to_internal_error)?;

    match state {
        GeneratorState::Executing | GeneratorState::AwaitingReturn => {
            let error = runtime
                .alloc_type_error("Generator is already running")
                .map_err(to_internal_error)?;
            Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ))
        }
        GeneratorState::Completed => {
            let result = runtime.create_iter_result(RegisterValue::undefined(), true)?;
            Ok(RegisterValue::from_object_handle(result.0))
        }
        GeneratorState::SuspendedStart | GeneratorState::SuspendedYield => {
            // Resume the generator via the RuntimeState resume entry point.
            runtime.resume_generator(generator, sent_value, GeneratorResumeKind::Next)
        }
    }
}

/// ES2024 §27.5.1.3 %GeneratorPrototype%.return(value)
/// Spec: <https://tc39.es/ecma262/#sec-generator.prototype.return>
fn generator_prototype_return(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let generator = require_generator_object(*this, runtime)?;
    let return_value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let state = runtime
        .objects()
        .generator_state(generator)
        .map_err(to_internal_error)?;

    match state {
        GeneratorState::Executing | GeneratorState::AwaitingReturn => {
            let error = runtime
                .alloc_type_error("Generator is already running")
                .map_err(to_internal_error)?;
            Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ))
        }
        GeneratorState::SuspendedStart | GeneratorState::Completed => {
            // Mark completed and return {value, done:true}
            let _ = runtime
                .objects_mut()
                .set_generator_state(generator, GeneratorState::Completed);
            let result = runtime.create_iter_result(return_value, true)?;
            Ok(RegisterValue::from_object_handle(result.0))
        }
        GeneratorState::SuspendedYield => {
            // Resume the generator with a return completion.
            runtime.resume_generator(generator, return_value, GeneratorResumeKind::Return)
        }
    }
}

/// ES2024 §27.5.1.4 %GeneratorPrototype%.throw(exception)
/// Spec: <https://tc39.es/ecma262/#sec-generator.prototype.throw>
fn generator_prototype_throw(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let generator = require_generator_object(*this, runtime)?;
    let exception = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let state = runtime
        .objects()
        .generator_state(generator)
        .map_err(to_internal_error)?;

    match state {
        GeneratorState::Executing | GeneratorState::AwaitingReturn => {
            let error = runtime
                .alloc_type_error("Generator is already running")
                .map_err(to_internal_error)?;
            Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ))
        }
        GeneratorState::SuspendedStart | GeneratorState::Completed => {
            let _ = runtime
                .objects_mut()
                .set_generator_state(generator, GeneratorState::Completed);
            Err(VmNativeCallError::Thrown(exception))
        }
        GeneratorState::SuspendedYield => {
            runtime.resume_generator(generator, exception, GeneratorResumeKind::Throw)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// The kind of resumption operation on a generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratorResumeKind {
    /// `.next(value)` — normal resumption.
    Next,
    /// `.return(value)` — return completion.
    Return,
    /// `.throw(value)` — throw completion.
    Throw,
}

fn require_generator_object(
    this: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        let error = runtime
            .alloc_type_error("Generator.prototype method called on non-object")
            .map_err(to_internal_error)?;
        return Err(VmNativeCallError::Thrown(
            RegisterValue::from_object_handle(error.0),
        ));
    };

    if !matches!(runtime.objects().kind(handle), Ok(HeapValueKind::Generator)) {
        let error = runtime
            .alloc_type_error("Generator.prototype method requires a generator object")
            .map_err(to_internal_error)?;
        return Err(VmNativeCallError::Thrown(
            RegisterValue::from_object_handle(error.0),
        ));
    }

    Ok(handle)
}

fn to_internal_error(error: impl std::fmt::Debug) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("generator internal error: {error:?}").into())
}

/// Installs a named method on a prototype object.
fn install_method(
    prototype: ObjectHandle,
    name: &str,
    arity: u16,
    f: NativeFn,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method(name, arity, f);
    let host_fn = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(host_fn, function_prototype)?;
    install_function_length_name(handle, arity, name, cx)?;
    let prop = cx.property_names.intern(name);
    cx.heap.define_own_property(
        prototype,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::builtin_method(),
        ),
    )?;
    Ok(())
}

/// Installs `@@toStringTag` as a non-writable, non-enumerable, configurable string.
fn install_to_string_tag(
    target: ObjectHandle,
    tag: &str,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let sym_tag = cx
        .property_names
        .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string(tag);
    cx.heap.define_own_property(
        target,
        sym_tag,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            // {W:false, E:false, C:true} per spec
            PropertyAttributes::from_flags(false, false, true),
        ),
    )?;
    Ok(())
}

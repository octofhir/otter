//! Integration coverage for the `couch!` class-intrinsic macro.

use otter_macros::couch;
use otter_vm::{NativeCtx, NativeError, Value, bootstrap, intrinsic_install};

fn macro_couch_ctor(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}

fn macro_couch_static_from(
    _ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    Ok(Value::number_i32(1))
}

fn macro_couch_static_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(2))
}

fn macro_couch_proto_method(
    _ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    Ok(Value::number_i32(3))
}

fn macro_couch_proto_get(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(4))
}

fn macro_couch_proto_set(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}

couch! {
    name = "MacroCouch",
    feature = CORE,
    constructor = (length = 2, call = macro_couch_ctor),
    statics = {
        "from" / 1 => macro_couch_static_from,
        "of"   / 0 => macro_couch_static_of,
    },
    prototype = {
        methods = {
            "valueOf" / 0 => macro_couch_proto_method,
        },
        accessors = [
            ("size", get = macro_couch_proto_get, set = macro_couch_proto_set),
        ],
    },
}

couch! {
    name = "MacroCouchAbstract",
    feature = CORE,
    intrinsic = AbstractIntrinsic,
    constructor = (length = 0, call = macro_couch_ctor, is_abstract = true),
}

#[test]
fn couch_emits_constructor_spec_with_listed_statics_and_prototype() {
    let spec = &MACROCOUCH_SPEC;
    assert_eq!(spec.name, "MacroCouch");
    assert_eq!(spec.length, 2);
    assert_eq!(spec.static_methods.len(), 2);
    assert_eq!(spec.static_methods[0].name, "from");
    assert_eq!(spec.static_methods[0].length, 1);
    assert_eq!(spec.static_methods[1].name, "of");
    assert_eq!(spec.static_methods[1].length, 0);
    assert_eq!(spec.prototype_methods.len(), 1);
    assert_eq!(spec.prototype_methods[0].name, "valueOf");
}

#[test]
fn couch_emits_builtin_intrinsic_adapter_with_matching_metadata() {
    assert_eq!(
        <Intrinsic as intrinsic_install::BuiltinIntrinsic>::NAME,
        "MacroCouch"
    );
    assert_eq!(
        <Intrinsic as intrinsic_install::BuiltinIntrinsic>::FEATURE,
        bootstrap::BootstrapFeatures::CORE
    );
}

#[test]
fn couch_abstract_emits_separate_intrinsic_with_zero_length_ctor() {
    let spec = &MACROCOUCHABSTRACT_SPEC;
    assert_eq!(spec.name, "MacroCouchAbstract");
    assert_eq!(spec.length, 0);
    assert_eq!(spec.static_methods.len(), 0);
    assert_eq!(spec.prototype_methods.len(), 0);
    assert_eq!(
        <AbstractIntrinsic as intrinsic_install::BuiltinIntrinsic>::NAME,
        "MacroCouchAbstract"
    );
}

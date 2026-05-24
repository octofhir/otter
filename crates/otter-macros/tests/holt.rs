//! Integration coverage for the `holt!` namespace-intrinsic macro.
//!
//! The macro is exercised against the live `otter-vm` crate so the
//! generated `BuiltinIntrinsic` adapter compiles and resolves
//! against the production trait + helper paths.

use otter_macros::holt;
use otter_vm::{NativeCtx, NativeError, Value, bootstrap, intrinsic_install};

fn macro_holt_abs(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(42))
}

fn macro_holt_id(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(args.first().copied().unwrap_or(Value::undefined()))
}

fn macro_holt_get_kind(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(7))
}

holt! {
    name = "MacroHolt",
    feature = CORE,
    constants = [
        ("PI",     Number(std::f64::consts::PI), read_only),
        ("FLAG",   Boolean(true)),
        ("NOTHING", Undefined),
    ],
    accessors = [
        ("kind", get = macro_holt_get_kind),
    ],
    methods = {
        "abs"     / 1 => macro_holt_abs,
        "id"      / 1 => macro_holt_id,
        // Per-row attrs override — emits with `Attr::data()` instead of
        // the default `builtin_function`.
        "visible" / 0 => macro_holt_abs attrs = data,
    },
}

#[test]
fn holt_emits_namespace_spec_with_listed_methods() {
    let spec = &MACROHOLT_SPEC;
    assert_eq!(spec.name, "MacroHolt");
    assert_eq!(spec.methods.len(), 3);
    assert_eq!(spec.methods[0].name, "abs");
    assert_eq!(spec.methods[0].length, 1);
    assert_eq!(spec.methods[1].name, "id");
    assert_eq!(spec.methods[2].name, "visible");
    // Per-row attrs override honoured: enumerable bit is true for
    // `Attr::data()`, false for the default `Attr::builtin_function()`.
    assert!(!spec.methods[0].attrs.enumerable);
    assert!(spec.methods[2].attrs.enumerable);
    assert_eq!(spec.constants.len(), 3);
    assert_eq!(spec.constants[0].name, "PI");
    assert_eq!(spec.constants[1].name, "FLAG");
    assert_eq!(spec.constants[2].name, "NOTHING");
    assert_eq!(spec.accessors.len(), 1);
    assert_eq!(spec.accessors[0].name, "kind");
    assert!(spec.accessors[0].get.is_some());
    assert!(spec.accessors[0].set.is_none());
}

#[test]
fn holt_emits_builtin_intrinsic_adapter_with_matching_metadata() {
    // `BuiltinIntrinsic` constants come through the generated impl.
    assert_eq!(
        <Intrinsic as intrinsic_install::BuiltinIntrinsic>::NAME,
        "MacroHolt"
    );
    assert_eq!(
        <Intrinsic as intrinsic_install::BuiltinIntrinsic>::FEATURE,
        bootstrap::BootstrapFeatures::CORE
    );
}

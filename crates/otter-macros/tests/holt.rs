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

holt! {
    name = "MacroHolt",
    feature = CORE,
    methods = {
        "abs" / 1 => macro_holt_abs,
        "id"  / 1 => macro_holt_id,
    },
}

#[test]
fn holt_emits_namespace_spec_with_listed_methods() {
    let spec = &MACROHOLT_SPEC;
    assert_eq!(spec.name, "MacroHolt");
    assert_eq!(spec.methods.len(), 2);
    assert_eq!(spec.methods[0].name, "abs");
    assert_eq!(spec.methods[0].length, 1);
    assert_eq!(spec.methods[1].name, "id");
    assert_eq!(spec.constants.len(), 0);
    assert_eq!(spec.accessors.len(), 0);
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

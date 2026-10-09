//! Stable safe callable ownership and fail-closed probe admission.

use super::*;

fn load_kernel(
    selection: JitSelection,
    source: &str,
) -> (Runtime, RuntimeExecutionContext, RootedCallable) {
    let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
    let (_, context) = runtime
        .run_script_with_context(SourceInput::from_javascript(source), "kernel.js")
        .unwrap();
    let callable = RootedCallable::load(&mut runtime, &context, "engineKernel").unwrap();
    (runtime, context, callable)
}

#[test]
fn repeated_calls_keep_the_original_closure_after_global_replacement_and_real_gc() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let (mut runtime, context, callable) = load_kernel(
            selection,
            r#"
            (() => {
                let calls = 0;
                let retained = { value: 7 };
                globalThis.engineKernel = () => {
                    calls++;
                    return retained.value + calls;
                };
            })();
        "#,
        );
        let root = callable.root;
        let function_id = callable.function_id;
        assert_eq!(callable.invoke(&mut runtime, &context).unwrap(), 8.0);
        runtime
            .run_script(
                SourceInput::from_javascript("globalThis.engineKernel = () => 999;"),
                "replacement.js",
            )
            .unwrap();
        runtime.force_gc().unwrap();
        assert_eq!(callable.invoke(&mut runtime, &context).unwrap(), 9.0);
        assert_eq!(callable.root, root);
        assert_eq!(callable.function_id, function_id);
        callable.remove(&mut runtime, &context).unwrap();
        assert!(
            callable.invoke(&mut runtime, &context).is_err(),
            "closed key cannot fall back to the replacement global"
        );
    }
}

#[test]
fn wrong_type_nonfinite_and_abrupt_results_cannot_supply_a_checksum() {
    for body in [
        "return '1';",
        "return NaN;",
        "return Infinity;",
        "throw new Error('kernel failed');",
    ] {
        let source = format!("globalThis.engineKernel = function () {{ {body} }};");
        let (mut runtime, context, callable) = load_kernel(JitSelection::InterpreterOnly, &source);
        assert!(callable.invoke(&mut runtime, &context).is_err(), "{body}");
        callable.remove(&mut runtime, &context).unwrap();
    }
}

#[test]
fn expected_checksum_requires_finite_values_and_preserves_zero_sign() {
    for text in ["NaN", "inf", "-inf", "Infinity", "nonnumeric"] {
        assert!(finite_checksum(text).is_err(), "{text}");
    }
    assert_eq!(finite_checksum("17.25").unwrap(), 17.25);
    assert!(checksum_matches(17.25, 17.25));
    assert!(!checksum_matches(-0.0, 0.0));
}

#[test]
fn every_retention_or_release_script_requires_its_explicit_expected_completion() {
    let base = [
        "probe",
        "kernel.js",
        "--expected",
        "1",
        "--trace",
        "trace.json",
    ];
    for flag in ["--validate-retained", "--release"] {
        assert!(Args::try_parse_from(base.into_iter().chain([flag, "phase.js"])).is_err());
    }
    for flag in ["--expected-retained", "--expected-released"] {
        assert!(Args::try_parse_from(base.into_iter().chain([flag, "1"])).is_err());
    }
    assert!(
        Args::try_parse_from(base.into_iter().chain([
            "--validate-retained",
            "validate.js",
            "--expected-retained",
            "17",
            "--release",
            "release.js",
            "--expected-released",
            "0",
        ]))
        .is_ok()
    );
}

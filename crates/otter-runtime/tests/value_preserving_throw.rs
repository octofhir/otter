//! Host natives can throw an arbitrary JavaScript value without losing it.
//!
//! DOM and other Web-IDL bindings need this for exception objects whose
//! identity and custom fields must survive the Rust native-call boundary.

use otter_runtime::{Runtime, RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, SourceInput};

fn throw_argument(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
) -> Result<RuntimeValue, RuntimeNativeError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RuntimeValue::undefined);
    Err(ctx.throw_value("hostThrow", value))
}

fn runtime() -> Runtime {
    let mut runtime = Runtime::builder().build().expect("runtime builds");
    runtime
        .install_native_global("hostThrow", 1, throw_argument)
        .expect("native global installs");
    runtime
}

fn eval(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .eval(SourceInput::from_javascript(source))
        .expect("script runs")
        .completion_string()
        .to_string()
}

#[test]
fn catch_observes_the_exact_object_passed_to_the_host() {
    let mut runtime = runtime();
    assert_eq!(
        eval(
            &mut runtime,
            "const exception = { name: 'NotFoundError', code: 8 };\n\
             let caught;\n\
             try { hostThrow(exception); } catch (error) { caught = error; }\n\
             caught === exception && caught.name === 'NotFoundError' && caught.code === 8",
        ),
        "true"
    );
}

#[test]
fn primitive_throw_values_are_preserved_too() {
    let mut runtime = runtime();
    assert_eq!(
        eval(
            &mut runtime,
            "let caught; try { hostThrow(42); } catch (error) { caught = error; } caught === 42",
        ),
        "true"
    );
}

#[test]
fn an_uncaught_host_value_keeps_its_diagnostic_cause() {
    let mut runtime = runtime();
    let error = runtime
        .eval(SourceInput::from_javascript(
            "hostThrow({ name: 'InvalidStateError', message: 'document is detached' })",
        ))
        .expect_err("the host value escapes the script");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("InvalidStateError") || rendered.contains("document is detached"),
        "the original thrown object is available to diagnostic enrichment: {rendered}"
    );

    assert_eq!(
        eval(&mut runtime, "1 + 1"),
        "2",
        "a host throw does not poison the isolate"
    );
}

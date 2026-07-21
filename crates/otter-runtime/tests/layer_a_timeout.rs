//! Layer A execution deadline tests.

use std::time::{Duration, Instant};

use otter_runtime::{OtterError, Runtime, SourceInput};

#[test]
fn infinite_loop_times_out_and_runtime_is_reusable() {
    let mut runtime = Runtime::builder()
        .timeout(Duration::from_millis(25))
        .build()
        .expect("runtime");
    let started = Instant::now();
    let error = runtime
        .eval(SourceInput::from_javascript("while (true) {}"))
        .expect_err("infinite loop must time out");
    assert!(matches!(error, OtterError::Timeout { .. }));
    assert!(started.elapsed() < Duration::from_secs(2));

    let result = runtime
        .eval(SourceInput::from_javascript("21 * 2"))
        .expect("runtime remains reusable after timeout");
    assert_eq!(result.completion_string(), "42");
    assert!(!runtime.interrupt_handle().is_interrupted());
}

#[test]
fn successful_execution_finishes_before_deadline() {
    let mut runtime = Runtime::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_typescript("const x: number = 6; x * 7"),
            "ok.ts",
        )
        .expect("script");
    assert_eq!(result.completion_string(), "42");
}

#[test]
fn module_graph_preparation_observes_deadline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_count = 48;
    for index in 0..module_count {
        let next = if index + 1 < module_count {
            format!("import {{ v as next }} from './m{}.js';\n", index + 1)
        } else {
            "const next = 0;\n".to_string()
        };
        let declarations = (0..400)
            .map(|n| format!("const local_{n} = {n};\n"))
            .collect::<String>();
        std::fs::write(
            temp.path().join(format!("m{index}.js")),
            format!("{next}{declarations}export const v = next + local_399;\n"),
        )
        .expect("module fixture");
    }

    let mut runtime = Runtime::builder()
        .timeout(Duration::from_millis(5))
        .build()
        .expect("runtime");
    let error = runtime
        .run_module(temp.path().join("m0.js"))
        .expect_err("graph preparation must observe timeout");
    assert!(matches!(error, OtterError::Timeout { .. }));
}

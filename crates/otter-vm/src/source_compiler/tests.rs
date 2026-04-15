//! Unit tests for the M0 scaffold of [`super::ModuleCompiler`].

use super::{ModuleCompiler, SourceLoweringError};
use oxc_span::SourceType;

#[test]
fn empty_source_is_unsupported() {
    let compiler = ModuleCompiler::new();
    let err = compiler
        .compile("", "empty.js", SourceType::default())
        .expect_err("M0 stub must reject all input");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "program",
            ..
        }
    ));
}

#[test]
fn simple_function_is_unsupported_in_m0() {
    let compiler = ModuleCompiler::new();
    let err = compiler
        .compile(
            "function f(n) { return n + 1; }",
            "m0.js",
            SourceType::default(),
        )
        .expect_err("M0 stub must reject functions until M1 lands");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "program",
            ..
        }
    ));
}

#[test]
fn syntax_error_reports_parse() {
    let compiler = ModuleCompiler::new();
    let err = compiler
        .compile("function (", "bad.js", SourceType::default())
        .expect_err("bad syntax must surface as Parse");
    assert!(matches!(err, SourceLoweringError::Parse { .. }));
}

#[test]
fn error_carries_nonempty_span_for_non_empty_input() {
    let compiler = ModuleCompiler::new();
    let err = compiler
        .compile("1 + 2;", "expr.js", SourceType::default())
        .expect_err("non-empty input still unsupported at M0");
    let span = err.span().expect("Unsupported span must be present");
    assert!(span.end > span.start || span.start == 0);
}

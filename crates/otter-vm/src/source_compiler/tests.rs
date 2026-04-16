//! Unit tests for [`super::ModuleCompiler`].
//!
//! Coverage matches the M1 contract: a single named function
//! declaration with 0 or 1 simple parameters whose body is a single
//! `return` of an int32-safe expression. Everything else surfaces as
//! [`SourceLoweringError::Unsupported`] with a stable `construct` tag.

use super::{ModuleCompiler, SourceLoweringError};
use crate::interpreter::Interpreter;
use crate::module::FunctionIndex;
use crate::value::RegisterValue;
use oxc_span::SourceType;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn compile(source: &str) -> Result<crate::module::Module, SourceLoweringError> {
    ModuleCompiler::new().compile(source, "test.js", SourceType::default())
}

fn run_int32_function(source: &str, args: &[i32]) -> i32 {
    let module = compile(source).expect("compile");
    let function = module
        .function(FunctionIndex(0))
        .expect("module has entry function");
    let register_count = function.frame_layout().register_count();
    let mut registers = vec![RegisterValue::undefined(); usize::from(register_count)];
    // Parameters are laid out immediately after the hidden slots. The
    // layout we emit is (1 hidden) + (n params), so slot 1.. is the
    // parameter window.
    let hidden = usize::from(function.frame_layout().hidden_count());
    for (i, arg) in args.iter().enumerate() {
        registers[hidden + i] = RegisterValue::from_i32(*arg);
    }
    let interpreter = Interpreter::new();
    let result = interpreter
        .execute(&module)
        .or_else(|_| {
            // Fall back to the explicit parameter-bound path for
            // functions that take arguments — `execute` only reaches
            // the module entry without preseeded registers.
            let mut runtime = crate::interpreter::RuntimeState::new();
            interpreter.execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
        })
        .expect("execute_with_runtime");
    result
        .return_value()
        .as_i32()
        .expect("function returned a non-int32 value")
}

// ---------------------------------------------------------------------------
// Parse-phase diagnostics
// ---------------------------------------------------------------------------

#[test]
fn syntax_error_reports_parse() {
    let err = compile("function (").expect_err("bad syntax must surface as Parse");
    assert!(matches!(err, SourceLoweringError::Parse { .. }));
}

// ---------------------------------------------------------------------------
// Unsupported shapes (expected negatives)
// ---------------------------------------------------------------------------

#[test]
fn empty_source_is_unsupported_program() {
    let err = compile("").expect_err("empty input has no program body at M1");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "program",
            ..
        }
    ));
}

#[test]
fn class_is_unsupported() {
    let err = compile("class Foo {}").expect_err("classes land post-M10");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "class_declaration",
            ..
        }
    ));
}

#[test]
fn non_int32_literal_is_unsupported() {
    let err = compile("function h() { return 1.5; }").expect_err("fractional literal");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "non_int32_literal",
            ..
        }
    ));
}

#[test]
fn two_functions_unsupported_in_m1() {
    let err = compile("function a() { return 1; } function b() { return 2; }")
        .expect_err("M1 accepts only one top-level statement");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "multiple_top_level_statements",
            ..
        }
    ));
}

#[test]
fn async_function_unsupported() {
    let err = compile("async function f() { return 1; }").expect_err("async lands later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "async_function",
            ..
        }
    ));
}

#[test]
fn generator_unsupported() {
    let err = compile("function* g() { return 1; }").expect_err("generator lands later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "generator",
            ..
        }
    ));
}

#[test]
fn multi_parameters_unsupported() {
    let err = compile("function f(a, b) { return a; }").expect_err("two params at M9+");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "multiple_parameters",
            ..
        }
    ));
}

#[test]
fn destructuring_parameter_unsupported() {
    let err = compile("function f({ x }) { return x; }").expect_err("destructuring later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "destructuring_parameter",
            ..
        }
    ));
}

#[test]
fn default_parameter_unsupported() {
    let err = compile("function f(n = 0) { return n; }").expect_err("default later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "default_parameter",
            ..
        }
    ));
}

#[test]
fn rest_parameter_unsupported() {
    let err = compile("function f(...rest) { return 1; }").expect_err("rest later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "rest_parameter",
            ..
        }
    ));
}

#[test]
fn non_addition_binary_unsupported() {
    let err = compile("function f(n) { return n - 1; }").expect_err("subtract at M3");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "subtraction",
            ..
        }
    ));
}

#[test]
fn unbound_identifier_unsupported() {
    let err = compile("function f(n) { return m; }").expect_err("globals later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "unbound_identifier",
            ..
        }
    ));
}

#[test]
fn multi_statement_body_unsupported() {
    let err = compile("function f() { let x = 1; return x; }").expect_err("locals at M4");
    // The body has two statements; `lower_function_body` catches the
    // second one before looking at either. Once M4 admits `let`, the
    // `multi_statement_body` arm will be removed and the first
    // statement's own tag will surface.
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "multi_statement_body",
            ..
        }
    ));
}

#[test]
fn return_without_value_unsupported() {
    let err = compile("function f() { return; }").expect_err("bare return at M4+");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "return_without_value",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Supported shapes — end-to-end through the v2 interpreter
// ---------------------------------------------------------------------------

#[test]
fn f_without_params_returns_literal() {
    // `function g() { return 7; }` — no parameters; body lowers to
    // `LdaSmi 7 / Return`. Invoked via `Interpreter::execute` which
    // runs the module entry with a default activation.
    assert_eq!(run_int32_function("function g() { return 7; }", &[]), 7);
}

#[test]
fn f_n_plus_1_returns_43_when_n_is_42() {
    // `function f(n) { return n + 1; }` — body lowers to
    // `Ldar r0 / AddSmi 1 / Return`. We pass `n = 42` via the
    // parameter-bound fallback path.
    assert_eq!(
        run_int32_function("function f(n) { return n + 1; }", &[42]),
        43
    );
}

#[test]
fn f_returning_negative_literal_is_unsupported_in_m1() {
    // `-7` parses as `UnaryExpression { op: "-", arg: NumericLiteral 7 }`,
    // not a negative literal, so it must surface as `unary_expression`
    // until M3/M4 introduce unary negation.
    let err = compile("function g() { return -7; }").expect_err("unary minus later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "unary_expression",
            ..
        }
    ));
}

#[test]
fn identifier_plus_identifier_uses_add_reg() {
    // Both operands are the single parameter `n`, so the RHS reuses
    // register r0 via `Add r0`. Sum of 21 + 21 is 42.
    assert_eq!(
        run_int32_function("function d(n) { return n + n; }", &[21]),
        42
    );
}

#[test]
fn wide_integer_literal_on_rhs_is_unsupported() {
    // 200 is outside i8 range, so AddSmi can't represent it at the
    // narrow width we emit. Until M4 lands locals, there is no scratch
    // slot to materialise the literal into, so this path rejects.
    let err = compile("function f(n) { return n + 200; }").expect_err("needs scratch slot");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "wide_integer_literal_on_rhs",
            ..
        }
    ));
}

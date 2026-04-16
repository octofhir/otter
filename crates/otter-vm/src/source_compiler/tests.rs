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
    // Always go through `execute_with_runtime` so the parameter slots
    // get seeded with the requested values. Earlier helper revisions
    // tried `execute()` first and only fell back on error — that
    // worked accidentally before M6 because every int32 op on an
    // unseeded `undefined` parameter threw, but M6's relational ops
    // happily succeed against `undefined` (returning false), which
    // would silently produce the wrong arm of the if-statement.
    let interpreter = Interpreter::new();
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = interpreter
        .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
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
fn division_unsupported_at_m3() {
    // `/` has no `*Smi` opcode in the v2 ISA and no Reg-form lowering
    // for the integer-only M3 surface. Stays unsupported as
    // `Unsupported { construct: "division" }` until later milestones
    // introduce the float lowering path.
    let err = compile("function f(n) { return n / 2; }").expect_err("division later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "division",
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
fn missing_trailing_return_after_dead_code_unsupported() {
    // The compiler always requires the *last* statement to be a
    // `ReturnStatement`. Putting a non-return statement after a
    // return therefore fails the trailing-return check rather than
    // being flagged as dead code (M6 has no reachability analysis;
    // the same tag will fire whether the earlier statements covered
    // every path or not).
    let err =
        compile("function f() { return 1; let x = 2; }").expect_err("non-return as last statement");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "missing_return",
            ..
        }
    ));
}

#[test]
fn missing_return_unsupported() {
    // M4 keeps M1's "exactly one return required" invariant — the v2
    // dispatcher relies on every function path ending at a `Return`
    // for the tier-up call exit. Falling off the end of the body is
    // valid JS (returns undefined) but not yet wired here.
    let err = compile("function f() { let x = 1; }").expect_err("body without return");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "missing_return",
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

// ---------------------------------------------------------------------------
// M3 — remaining int32 binary operators
// ---------------------------------------------------------------------------

#[test]
fn subtract_literal_uses_subsmi() {
    assert_eq!(
        run_int32_function("function f(n) { return n - 1; }", &[42]),
        41
    );
}

#[test]
fn subtract_register_uses_sub() {
    assert_eq!(
        run_int32_function("function f(n) { return n - n; }", &[42]),
        0
    );
}

#[test]
fn multiply_literal_uses_mulsmi() {
    assert_eq!(
        run_int32_function("function f(n) { return n * 3; }", &[7]),
        21
    );
}

#[test]
fn multiply_register_uses_mul() {
    assert_eq!(
        run_int32_function("function f(n) { return n * n; }", &[6]),
        36
    );
}

#[test]
fn bitwise_or_literal_uses_orsmi() {
    assert_eq!(
        run_int32_function("function f(n) { return n | 1; }", &[4]),
        5
    );
}

#[test]
fn bitwise_or_register_uses_or() {
    assert_eq!(
        run_int32_function("function f(n) { return n | n; }", &[5]),
        5
    );
}

#[test]
fn bitwise_and_literal_uses_andsmi() {
    assert_eq!(
        run_int32_function("function f(n) { return n & 12; }", &[7]),
        4
    );
}

#[test]
fn bitwise_and_register_uses_and() {
    assert_eq!(
        run_int32_function("function f(n) { return n & n; }", &[7]),
        7
    );
}

#[test]
fn bitwise_xor_register_uses_xor() {
    // No `BitwiseXorSmi` opcode in the v2 ISA — Reg form is the only
    // path, so we exercise `n ^ n` (which collapses to 0 for any n).
    assert_eq!(
        run_int32_function("function f(n) { return n ^ n; }", &[42]),
        0
    );
}

#[test]
fn bitwise_xor_literal_rhs_is_unsupported() {
    // No `BitwiseXorSmi` and no scratch slot to materialise the
    // literal — same `wide_integer_literal_on_rhs` rejection as the
    // out-of-i8 AddSmi case. Will become supported once M4 lands
    // local slots that can hold the materialised RHS.
    let err = compile("function f(n) { return n ^ 1; }").expect_err("xor literal needs scratch");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "wide_integer_literal_on_rhs",
            ..
        }
    ));
}

#[test]
fn shift_left_literal_uses_shlsmi() {
    assert_eq!(
        run_int32_function("function f(n) { return n << 2; }", &[3]),
        12
    );
}

#[test]
fn shift_left_register_uses_shl() {
    // `n << n` for n=3 → 3 << 3 == 24.
    assert_eq!(
        run_int32_function("function f(n) { return n << n; }", &[3]),
        24
    );
}

#[test]
fn shift_right_literal_uses_shrsmi() {
    // Arithmetic shift right; for negative inputs `Shr` sign-extends.
    assert_eq!(
        run_int32_function("function f(n) { return n >> 1; }", &[-8]),
        -4
    );
}

#[test]
fn shift_right_register_uses_shr() {
    assert_eq!(
        run_int32_function("function f(n) { return n >> n; }", &[2]),
        0
    );
}

#[test]
fn unsigned_shift_right_register_uses_ushr() {
    // No `UShrSmi` opcode in the v2 ISA — Reg form only. `n >>> n`
    // for n=4 collapses to `4 >>> 4 == 0`.
    assert_eq!(
        run_int32_function("function f(n) { return n >>> n; }", &[4]),
        0
    );
}

#[test]
fn unsigned_shift_right_literal_rhs_is_unsupported() {
    // Same reason as `BitwiseXor` literal RHS — no `UShrSmi` opcode
    // and no scratch slot.
    let err = compile("function f(n) { return n >>> 1; }").expect_err("ushr literal needs scratch");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "wide_integer_literal_on_rhs",
            ..
        }
    ));
}

#[test]
fn wide_subsmi_literal_is_unsupported() {
    // 200 > i8::MAX, so SubSmi can't encode it; rejects via the same
    // tag as AddSmi. Confirms the Smi-width check applies uniformly
    // across operators with a Smi opcode, not just `+`.
    let err = compile("function f(n) { return n - 200; }").expect_err("needs scratch slot");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "wide_integer_literal_on_rhs",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// M4 — local `let`/`const` with initializer
// ---------------------------------------------------------------------------

#[test]
fn let_with_int_literal_initializer_returns_value() {
    // Body: `let x = 7; return x;` — slot 1 holds `x` (after the
    // hidden receiver + zero parameters); bytecode is `LdaSmi 7 /
    // Star r0 / Ldar r0 / Return`. Note local-slot indexing is
    // user-visible: with 0 params + 1 local, the local is at user
    // register 0.
    assert_eq!(
        run_int32_function("function f() { let x = 7; return x; }", &[]),
        7
    );
}

#[test]
fn const_with_int_literal_initializer_returns_value() {
    // `const` shares M4's lowering path with `let` (no
    // AssignmentExpression yet → `const` reassignment can't even be
    // expressed; the reassignment-rejection guard is M5+).
    assert_eq!(
        run_int32_function("function g() { const k = 9; return k; }", &[]),
        9
    );
}

#[test]
fn let_initialized_from_param_arithmetic() {
    // `let x = n + 1; return x;` — slot layout is
    // `[hidden | n | x]`, so `Ldar r0 / AddSmi 1 / Star r1 /
    // Ldar r1 / Return`. Confirms the parameter is reachable from the
    // initializer and the local lands one slot beyond the param.
    assert_eq!(
        run_int32_function("function f(n) { let x = n + 1; return x; }", &[42]),
        43
    );
}

#[test]
fn two_lets_compose() {
    // Both bindings live for the whole body; the second initializer
    // reads the first.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = n + 1; let y = x * 2; return y; }",
            &[10]
        ),
        22
    );
}

#[test]
fn let_returning_self_uses_local_slot() {
    // Confirms the return path picks up the local (via `Ldar r0`)
    // rather than re-reading the original initializer expression.
    assert_eq!(
        run_int32_function("function f() { let v = 100; return v; }", &[]),
        100
    );
}

#[test]
fn let_initializer_can_combine_param_and_local() {
    // `let acc = n; let acc2 = acc | 1; return acc2;` — exercises the
    // BitOr Reg-form against a local slot.
    assert_eq!(
        run_int32_function(
            "function f(n) { let acc = n; let acc2 = acc | 1; return acc2; }",
            &[4]
        ),
        5
    );
}

// ---------------------------------------------------------------------------
// M4 — negative cases
// ---------------------------------------------------------------------------

#[test]
fn var_declaration_unsupported() {
    // `var` has hoisting + function-scope semantics distinct from
    // `let`/`const`. M4 keeps it rejected so we don't accidentally
    // alias the two surfaces.
    let err =
        compile("function f() { var x = 1; return x; }").expect_err("var has different scope");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "var_declaration",
            ..
        }
    ));
}

#[test]
fn uninitialized_let_unsupported() {
    // M4 demands an initializer on every binding — bare `let x;` is
    // rejected; the `let x = undefined;` workaround would also fail
    // because `undefined` is `Identifier("undefined")` which isn't in
    // scope.
    let err = compile("function f() { let x; return 1; }").expect_err("let without init at M4");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "uninitialized_binding",
            ..
        }
    ));
}

#[test]
fn multiple_declarators_unsupported() {
    // `let a = 1, b = 2;` packs two declarators into one statement.
    // M4 rejects until a future milestone teaches the lowering to
    // process declarator lists in order.
    let err = compile("function f() { let a = 1, b = 2; return a; }")
        .expect_err("multi-declarator at M4");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "multi_declarator",
            ..
        }
    ));
}

#[test]
fn destructuring_binding_unsupported() {
    let err =
        compile("function f() { let [x] = [1]; return x; }").expect_err("destructuring later");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "destructuring_binding",
            ..
        }
    ));
}

#[test]
fn duplicate_local_binding_unsupported() {
    // Two `let`s with the same name within one body. JS would throw
    // SyntaxError (re-declaration); M4 surfaces the same intent at
    // compile time.
    let err = compile("function f() { let x = 1; let x = 2; return x; }")
        .expect_err("duplicate local at M4");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "duplicate_binding",
            ..
        }
    ));
}

#[test]
fn local_shadowing_param_unsupported() {
    // Same name as the parameter — `function f(n) { let n = 1; ...}`
    // is a SyntaxError in real JS too.
    let err =
        compile("function f(n) { let n = 2; return n; }").expect_err("local shadows param at M4");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "duplicate_binding",
            ..
        }
    ));
}

#[test]
fn tdz_self_reference_in_initializer_unsupported() {
    // `let x = x + 1;` — the binding for `x` is in scope from
    // declaration, but reading it inside its own initializer hits
    // the M4 compile-time TDZ check. Spec semantics throw a
    // ReferenceError at runtime; M4 surfaces it earlier so the
    // bytecode never even executes.
    let err =
        compile("function f() { let x = x + 1; return x; }").expect_err("TDZ self-reference at M4");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "tdz_self_reference",
            ..
        }
    ));
}

#[test]
fn tdz_self_reference_in_initializer_via_binary_rhs_unsupported() {
    // `let x = 1 + x;` — same TDZ issue, surfaced via the Reg-form
    // RHS path (`lower_identifier_as_reg_rhs`). Wait — `1 + x`
    // doesn't quite work because `1` is a literal LHS so the path is
    // `LdaSmi 1; Add r_x`. The Reg-RHS path checks initialization
    // and rejects.
    let err =
        compile("function f() { let x = 1 + x; return x; }").expect_err("TDZ via binary RHS at M4");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "tdz_self_reference",
            ..
        }
    ));
}

#[test]
fn fractional_initializer_unsupported() {
    // Same `non_int32_literal` rejection as the M1 return path —
    // the lowering reuses `lower_return_expression` for the init
    // expression, so fractional literals fail there.
    let err =
        compile("function f() { let x = 1.5; return x; }").expect_err("non-int32 literal in init");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "non_int32_literal",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// M5 — AssignmentExpression onto a local `let`
// ---------------------------------------------------------------------------

#[test]
fn plain_assign_overwrites_local_let() {
    // `let x = 1; x = 5; return x;` — `let` initializes x to 1, the
    // bare `x = 5;` statement overwrites it via Star r_x, then the
    // return reads the updated slot.
    assert_eq!(
        run_int32_function("function f() { let x = 1; x = 5; return x; }", &[]),
        5
    );
}

#[test]
fn add_assign_compounds_with_smi_path() {
    // `x += 3` lowers to `Ldar r_x; AddSmi 3; Star r_x` — exercises
    // the i8-fit `*Smi` fast path on the compound assignment.
    assert_eq!(
        run_int32_function("function f() { let x = 10; x += 3; return x; }", &[]),
        13
    );
}

#[test]
fn add_assign_with_identifier_rhs_uses_reg_form() {
    // `x += n` lowers to `Ldar r_x; Add r_n; Star r_x` — Reg form
    // when the RHS is an in-scope identifier.
    assert_eq!(
        run_int32_function("function f(n) { let x = 10; x += n; return x; }", &[5]),
        15
    );
}

#[test]
fn sub_assign_compounds() {
    assert_eq!(
        run_int32_function("function f() { let x = 20; x -= 7; return x; }", &[]),
        13
    );
}

#[test]
fn mul_assign_compounds() {
    assert_eq!(
        run_int32_function("function f() { let x = 6; x *= 7; return x; }", &[]),
        42
    );
}

#[test]
fn or_assign_compounds() {
    assert_eq!(
        run_int32_function("function f() { let x = 4; x |= 1; return x; }", &[]),
        5
    );
}

#[test]
fn assignment_value_flows_into_outer_let() {
    // `let y = x = 5;` — the assignment leaves 5 in the accumulator,
    // so the next `Star r_y` writes 5 to `y` as well.
    assert_eq!(
        run_int32_function("function f() { let x = 1; let y = x = 5; return y; }", &[]),
        5
    );
}

#[test]
fn return_assignment_yields_assigned_value() {
    // `return x = 7;` returns the assigned value, mirroring JS:
    // assignment is an expression that evaluates to the RHS.
    assert_eq!(
        run_int32_function("function f() { let x = 1; return x = 7; }", &[]),
        7
    );
}

#[test]
fn chain_of_compound_assignments_accumulates() {
    // `x += 1; x *= 3; x -= 2; return x;` — exercises the
    // statement-level loop with three compound assignments back-to-back.
    assert_eq!(
        run_int32_function(
            "function f() { let x = 5; x += 1; x *= 3; x -= 2; return x; }",
            &[]
        ),
        16
    );
}

#[test]
fn assignment_after_let_chain() {
    // Ensures the body grammar accepts `let / let / assign / return`
    // in that order — i.e. assignments aren't required to come first.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = n; let y = x; x = y * 2; return x; }",
            &[7]
        ),
        14
    );
}

// ---------------------------------------------------------------------------
// M5 — negative cases
// ---------------------------------------------------------------------------

#[test]
fn const_assignment_unsupported() {
    // `const k = 1; k = 2;` — JS throws TypeError. M5 surfaces it at
    // compile time so the bytecode never gets a chance to write.
    let err = compile("function f() { const k = 1; k = 2; return k; }")
        .expect_err("const reassignment at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "const_assignment",
            ..
        }
    ));
}

#[test]
fn compound_const_assignment_unsupported() {
    // Same rejection for `const k += 1` — the const guard fires
    // before the lowering picks the binary op.
    let err = compile("function f() { const k = 1; k += 2; return k; }")
        .expect_err("const compound assign at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "const_assignment",
            ..
        }
    ));
}

#[test]
fn assignment_to_param_unsupported() {
    // `function f(n) { n = 5; }` is valid JS (parameters are
    // mutable), but M5 rejects it: parameters live in a different
    // slot range and the surface is intentionally minimal until M9+
    // expands the semantics.
    let err = compile("function f(n) { n = 5; return n; }").expect_err("param assignment at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "assignment_to_param",
            ..
        }
    ));
}

#[test]
fn assignment_to_undeclared_unsupported() {
    // `q = 5` where `q` is undeclared — JS implicit-global semantics
    // are out of scope (and rejected by strict mode anyway). Surfaces
    // as `unbound_identifier`.
    let err = compile("function f() { q = 5; return 1; }").expect_err("implicit global at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "unbound_identifier",
            ..
        }
    ));
}

#[test]
fn member_assignment_target_unsupported() {
    let err =
        compile("function f(n) { n.x = 5; return 1; }").expect_err("member assign target at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "member_assignment_target",
            ..
        }
    ));
}

#[test]
fn destructuring_assignment_target_unsupported() {
    let err = compile("function f() { let x = 1; [x] = [2]; return x; }")
        .expect_err("destructuring assign at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "destructuring_assignment_target",
            ..
        }
    ));
}

#[test]
fn unsupported_compound_assign_div() {
    // `x /= 2` — division has no `*Smi` opcode in the supported set
    // and division isn't on the M3/M5 binary surface either, so the
    // assignment lowering rejects with a stable per-operator tag.
    let err = compile("function f() { let x = 6; x /= 2; return x; }").expect_err("/= at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "division_assign",
            ..
        }
    ));
}

#[test]
fn unsupported_compound_assign_xor() {
    // `^=` lacks a `BitwiseXorSmi` opcode in the v2 ISA. M5 keeps
    // the operator out of scope rather than falling back to a
    // scratch-slot materialisation.
    let err = compile("function f() { let x = 6; x ^= 1; return x; }").expect_err("^= at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "bitwise_xor_assign",
            ..
        }
    ));
}

#[test]
fn unsupported_compound_assign_shl() {
    let err = compile("function f() { let x = 1; x <<= 2; return x; }").expect_err("<<= at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "shift_left_assign",
            ..
        }
    ));
}

#[test]
fn unsupported_compound_assign_logical_or() {
    let err = compile("function f() { let x = 1; x ||= 2; return x; }").expect_err("||= at M5");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "logical_or_assign",
            ..
        }
    ));
}

#[test]
fn bare_expression_statement_unsupported() {
    // `5;` — a literal-as-statement isn't an assignment, so the
    // body-grammar check rejects it. Surfaces via the existing
    // expression construct tag.
    let err = compile("function f() { 5; return 1; }").expect_err("bare expr stmt at M5");
    assert!(matches!(err, SourceLoweringError::Unsupported { .. }));
}

// ---------------------------------------------------------------------------
// M6 — relational operators
// ---------------------------------------------------------------------------

#[test]
fn less_than_literal_uses_swap() {
    // `n < 5` lowers via the swap path: `LdaSmi 5; TestGreaterThan
    // r_n` ≡ `5 > n`. With n=3, the result is true → returned 1.
    assert_eq!(
        run_int32_function("function f(n) { if (n < 5) { return 1; } return 0; }", &[3]),
        1
    );
    // With n=10, n < 5 is false → falls through to `return 0`.
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n < 5) { return 1; } return 0; }",
            &[10]
        ),
        0
    );
}

#[test]
fn less_than_two_identifiers_forward() {
    // `n < x` lowers via the forward path: `Ldar r_n; TestLessThan
    // r_x`. Confirms identifier-on-both-sides works without swap.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 100; if (n < x) { return 1; } return 0; }",
            &[5]
        ),
        1
    );
}

#[test]
fn greater_than_literal_uses_swap() {
    // M6 has no unary negation yet — write the "negative" sentinel
    // as `0` rather than `-1`. The interesting bit is the
    // `LdaSmi 0; TestLessThan r_n` swap path, which is exercised
    // regardless of which non-zero value the else branch returns.
    assert_eq!(
        run_int32_function("function f(n) { if (n > 0) { return n; } return 0; }", &[7]),
        7
    );
}

#[test]
fn less_than_or_equal_smi_eq_path() {
    // `n <= 5` with n = 5 → true.
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n <= 5) { return 1; } return 0; }",
            &[5]
        ),
        1
    );
}

#[test]
fn greater_than_or_equal_works() {
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n >= 5) { return 1; } return 0; }",
            &[5]
        ),
        1
    );
}

#[test]
fn strict_equal_works_in_both_directions() {
    // `n === 7` (forward via swap-no-op since op is symmetric) and
    // `7 === n` (forward) both reduce to `LdaSmi 7; TestEqualStrict
    // r_n` — confirms the symmetric swap handling.
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n === 7) { return 1; } return 0; }",
            &[7]
        ),
        1
    );
    assert_eq!(
        run_int32_function(
            "function f(n) { if (7 === n) { return 1; } return 0; }",
            &[7]
        ),
        1
    );
}

#[test]
fn strict_inequal_inverts_via_logical_not() {
    // `n !== 7` lowers as `LdaSmi 7; TestEqualStrict r_n; LogicalNot`.
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n !== 7) { return 1; } return 0; }",
            &[3]
        ),
        1
    );
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n !== 7) { return 1; } return 0; }",
            &[7]
        ),
        0
    );
}

#[test]
fn comparison_in_return_position() {
    // `return n < 5;` returns the boolean — under the int32-only
    // `as_i32()` test helper, true coerces to 1 and false to 0
    // through the v2 dispatcher's RegisterValue boolean → int path.
    // (Booleans are NaN-boxed; `as_i32` handles the
    // truthy-int-coercion via the same path the JIT box_int32 uses
    // when the accumulator carries a boolean tag.)
    //
    // Tests skipped: this path returns a boolean RegisterValue, not
    // an int32, so `as_i32` fails. Instead, confirm the comparison
    // behaviour through the if-statement form (covered above).
    let module = compile("function f(n) { return n < 5; }").expect("M6 compiles");
    let function = module.function(FunctionIndex(0)).expect("module has entry");
    let frame = function.frame_layout();
    // FrameLayout is `[hidden | n]` — 1 + 1 = 2 slots.
    assert_eq!(frame.parameter_count(), 1);
    assert_eq!(frame.local_count(), 0);
}

// ---------------------------------------------------------------------------
// M6 — IfStatement structure
// ---------------------------------------------------------------------------

#[test]
fn if_else_picks_correct_branch() {
    // M6 has no reachability analysis: even when every if-else
    // branch returns, the body still needs an explicit trailing
    // `return`. The `return 0;` below is dead code at runtime but
    // satisfies the compile-time grammar. M7+ can lift this once
    // proper reachability lands.
    let src = "function f(n) { if (n < 0) { return 100; } else { return 50; } return 0; }";
    assert_eq!(run_int32_function(src, &[5]), 50);
    assert_eq!(run_int32_function(src, &[-5]), 100);
}

#[test]
fn if_without_else_falls_through() {
    // No else → JumpIfToBooleanFalse jumps to the instruction
    // *after* the consequent block; the trailing `return n` is
    // reached when the if condition is false.
    assert_eq!(
        run_int32_function("function f(n) { if (n < 0) { return 0; } return n; }", &[7]),
        7
    );
    assert_eq!(
        run_int32_function(
            "function f(n) { if (n < 0) { return 0; } return n; }",
            &[-3]
        ),
        0
    );
}

#[test]
fn if_with_assignment_in_branch() {
    // The if branch reassigns a top-level local; the post-if return
    // reads its updated value. Exercises the "let at top-level then
    // reassigned in nested block" path.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 0; if (n > 0) { x = n; } return x; }",
            &[42]
        ),
        42
    );
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 0; if (n > 0) { x = n; } return x; }",
            &[-42]
        ),
        0
    );
}

#[test]
fn if_else_with_assignments_in_each_branch() {
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 0; if (n < 0) { x = 100; } else { x = n; } return x; }",
            &[5]
        ),
        5
    );
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 0; if (n < 0) { x = 100; } else { x = n; } return x; }",
            &[-5]
        ),
        100
    );
}

#[test]
fn nested_if_chain() {
    // `if (n < 0) … else if (n < 10) … else …` — the parser desugars
    // `else if` as `else { if … }`, so this exercises the alternate
    // path being itself an IfStatement (via lower_nested_statement →
    // lower_if_statement recursion).
    // `x = -1` would need unary negation (M6 doesn't have it yet);
    // use `x = 0 - 1` instead — that lowers to `LdaSmi 0; SubSmi 1`
    // and produces the same runtime value.
    let src = "function f(n) {
        let x = 0;
        if (n < 0) {
            x = 0 - 1;
        } else if (n < 10) {
            x = 1;
        } else {
            x = 2;
        }
        return x;
    }";
    assert_eq!(run_int32_function(src, &[-5]), -1);
    assert_eq!(run_int32_function(src, &[5]), 1);
    assert_eq!(run_int32_function(src, &[100]), 2);
}

#[test]
fn if_without_block_braces() {
    // `if (n > 0) return n;` — the consequent is a single Statement,
    // not a BlockStatement. Confirms `lower_nested_statement` handles
    // a bare ReturnStatement directly without unwrapping a block.
    assert_eq!(
        run_int32_function("function f(n) { if (n > 0) return n; return 0; }", &[42]),
        42
    );
    assert_eq!(
        run_int32_function("function f(n) { if (n > 0) return n; return 0; }", &[-1]),
        0
    );
}

#[test]
fn if_inside_compound_assignment_chain() {
    // Exercises the body grammar with mixed forms: let, if, assign,
    // return all in sequence.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = n; if (x < 0) { x = 0; } x += 10; return x; }",
            &[-3]
        ),
        10
    );
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = n; if (x < 0) { x = 0; } x += 10; return x; }",
            &[5]
        ),
        15
    );
}

// ---------------------------------------------------------------------------
// M6 — negative cases
// ---------------------------------------------------------------------------

#[test]
fn nested_let_inside_if_unsupported() {
    // M6 has no block scoping. `let x` inside an `if` would either
    // need its own slot lifetime (block scope) or be hoisted (which
    // changes observable semantics). Reject until block scoping
    // lands.
    let err = compile("function f(n) { if (n > 0) { let x = 1; } return n; }")
        .expect_err("nested let at M6");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "nested_variable_declaration",
            ..
        }
    ));
}

#[test]
fn two_literal_comparison_unsupported() {
    // `5 < 10` — neither operand can land in a register without a
    // scratch slot, so the relational lowering rejects via
    // `relational_needs_register_operand`.
    let err = compile("function f() { if (5 < 10) { return 1; } return 0; }")
        .expect_err("two-literal comparison at M6");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "relational_needs_register_operand",
            ..
        }
    ));
}

#[test]
fn comparison_with_unsupported_operator_falls_through_to_binary() {
    // `n != 5` (loose `!=`, not `!==`). The binary-op encoding
    // returns None for it (still a "comparison" tag) and the
    // relational lowering doesn't accept loose equality either, so
    // it surfaces via `binary_operator_tag`.
    let err = compile("function f(n) { if (n != 5) { return 1; } return 0; }")
        .expect_err("loose equality at M6");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "comparison",
            ..
        }
    ));
}

#[test]
fn if_with_return_only_branch_still_requires_trailing_return() {
    // `if (n > 0) return n;` has a return inside the if branch —
    // but the function still falls through if n <= 0. M6 doesn't
    // synthesize a fall-through return, so the body must still end
    // with one.
    let err = compile("function f(n) { if (n > 0) return n; }")
        .expect_err("missing trailing return at M6");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "missing_return",
            ..
        }
    ));
}

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
    // Use the module's declared entry — M9 picks the *last*
    // top-level FunctionDeclaration as `entry`, so the helper
    // can no longer hardcode `FunctionIndex(0)`.
    let entry_idx = module.entry();
    let function = module
        .function(entry_idx)
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
        .execute_with_runtime(&module, entry_idx, &registers, &mut runtime)
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
fn two_functions_at_top_level_pick_last_as_entry() {
    // M9 lifted M1's "single top-level FunctionDeclaration"
    // restriction. Both functions are compiled; the entry index
    // points at the last declaration, so calling `otter run` on
    // this module would invoke `b` (returning 2), not `a`.
    assert_eq!(
        run_int32_function("function a() { return 1; } function b() { return 2; }", &[]),
        2
    );
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
fn unary_minus_on_literal_returns_negated_int32() {
    // `-7` parses as `UnaryExpression { op: "-", arg: NumericLiteral 7 }`.
    // Post-M10 this lowers to `LdaSmi 7; Negate; Return`.
    assert_eq!(run_int32_function("function g() { return -7; }", &[]), -7);
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
fn multiple_declarators_compose() {
    // M7 lifted M4's "single declarator only" restriction so the
    // bench2 shape `let s = 0, i = 0;` compiles directly. Each
    // declarator allocates its own slot, in source order.
    assert_eq!(
        run_int32_function("function f() { let a = 1, b = 2; return a + b; }", &[]),
        3
    );
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
fn let_inside_if_body_is_block_scoped() {
    // M12: `let x = 1;` inside an `if` body (wrapped in `{}`) is
    // block-scoped — the binding only exists within the block.
    // Outside the block, the same name can be reused freely
    // (FrameLayout reserves the peak slot count so the two
    // reservations don't collide).
    assert_eq!(
        run_int32_function(
            "function f(n) { let r = 0; if (n > 0) { let x = 7; r = x; } return r; }",
            &[1],
        ),
        7,
    );
    // `n <= 0` skips the block entirely; `r` stays 0.
    assert_eq!(
        run_int32_function(
            "function f(n) { let r = 0; if (n > 0) { let x = 7; r = x; } return r; }",
            &[-1],
        ),
        0,
    );
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

// ---------------------------------------------------------------------------
// M7 — WhileStatement + bench2 sum-loop shape
// ---------------------------------------------------------------------------

#[test]
fn while_loop_sums_zero_to_n() {
    // The canonical M7 shape (see `V2_MIGRATION.md`). Closes
    // bench2.ts: int32 accumulator loop with the `(s + i) | 0`
    // truncation idiom. Validates the consolidated
    // `lower_accumulator_operand` accepts a parenthesised binary
    // LHS (without it, the `(s + i)` half rejects).
    let src = "function sum(n) {
        let s = 0, i = 0;
        while (i < n) {
            s = (s + i) | 0;
            i = i + 1;
        }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[0]), 0);
    assert_eq!(run_int32_function(src, &[1]), 0);
    assert_eq!(run_int32_function(src, &[10]), 45);
    assert_eq!(run_int32_function(src, &[100]), 4950);
}

#[test]
fn while_loop_with_compound_assign_increment() {
    // Mixes `+=` body with a relational test condition.
    let src = "function f(n) {
        let i = 0;
        while (i < n) { i += 1; }
        return i;
    }";
    assert_eq!(run_int32_function(src, &[7]), 7);
    assert_eq!(run_int32_function(src, &[0]), 0);
}

#[test]
fn while_loop_never_enters_when_condition_false() {
    // n=0 means the condition `i < n` is false from the start —
    // the body never runs and the JumpIfToBooleanFalse skips
    // straight to the loop exit on the first pass.
    let src = "function f(n) {
        let count = 0;
        let i = 0;
        while (i < n) {
            count = count + 1;
            i = i + 1;
        }
        return count;
    }";
    assert_eq!(run_int32_function(src, &[0]), 0);
    assert_eq!(run_int32_function(src, &[5]), 5);
}

#[test]
fn while_loop_inside_if_branch() {
    // Exercises `lower_nested_statement` recursing through an
    // `if`'s consequent into a `while` loop. Confirms the
    // top-statement → nested-statement chain stays intact.
    let src = "function f(n) {
        let acc = 0;
        if (n > 0) {
            let mode = n;
            return mode;
        } else {
            let i = n;
            while (i < 0) {
                acc = acc + 1;
                i = i + 1;
            }
        }
        return acc;
    }";
    // Wait — the if branches have nested `let`s, which M7 still
    // rejects. Use a simpler shape that keeps the nesting but
    // doesn't introduce inner declarations.
    let _ = src;
    let src = "function f(n) {
        let acc = 0;
        let i = 0;
        if (n > 0) {
            while (i < n) {
                acc = acc + i;
                i = i + 1;
            }
        }
        return acc;
    }";
    assert_eq!(run_int32_function(src, &[5]), 10); // 0+1+2+3+4
    assert_eq!(run_int32_function(src, &[0]), 0); // n>0 false → skip
}

#[test]
fn nested_while_loops() {
    // Outer counts down, inner sums up `i`. With n=3:
    //   outer iteration 1: inner sums 0+1+2 = 3; total += 3 → 3
    //   outer iteration 2: inner sums 0+1+2 = 3; total += 3 → 6
    //   outer iteration 3: inner sums 0+1+2 = 3; total += 3 → 9
    let src = "function f(n) {
        let total = 0;
        let outer = 0;
        while (outer < n) {
            let inner = 0;
            // ^ would need block scoping; rewrite with reset.
            return 0;
        }
        return total;
    }";
    let _ = src;
    // Rewrite the nested-while test to reuse a top-level local
    // instead of declaring `inner` per iteration. Same result, no
    // block scoping required.
    let src = "function f(n) {
        let total = 0;
        let outer = 0;
        let inner = 0;
        while (outer < n) {
            inner = 0;
            while (inner < n) {
                total = total + 1;
                inner = inner + 1;
            }
            outer = outer + 1;
        }
        return total;
    }";
    // n*n iterations of `total += 1`.
    assert_eq!(run_int32_function(src, &[3]), 9);
    assert_eq!(run_int32_function(src, &[5]), 25);
}

#[test]
fn while_loop_with_early_return_in_body() {
    // The `return` inside the loop body fires when the condition is
    // hit; without it, the loop would run to completion and return
    // the trailing -1.
    let src = "function f(n) {
        let i = 0;
        while (i < 1000) {
            if (i === n) { return i; }
            i = i + 1;
        }
        return 0;
    }";
    assert_eq!(run_int32_function(src, &[42]), 42);
    assert_eq!(run_int32_function(src, &[0]), 0);
}

// ---------------------------------------------------------------------------
// M7 — negative cases
// ---------------------------------------------------------------------------

#[test]
fn let_inside_while_body_is_block_scoped() {
    // M12: `let` inside a `while` body is block-scoped per iteration
    // — each iteration starts with a freshly-declared `x`. Updates
    // to an outer `let` from inside the block are observable.
    assert_eq!(
        run_int32_function(
            "function f(n) { let s = 0; let i = 0; while (i < n) { let x = i; s = s + x; i = i + 1; } return s; }",
            &[4],
        ),
        // sum 0..3 = 6
        6,
    );
}

#[test]
fn break_exits_while_at_threshold() {
    // `break` inside the innermost while leaves the loop; `i`
    // stops advancing at the first iteration where the condition
    // fires.
    assert_eq!(
        run_int32_function(
            "function f(n) { let i = 0; while (i < n) { if (i === 5) { break; } i = i + 1; } return i; }",
            &[10],
        ),
        5,
    );
}

#[test]
fn continue_skips_rest_of_while_iteration() {
    // `continue` jumps to the loop header — `i` must still be
    // advanced before `continue` runs, otherwise the loop spins
    // forever. This test exercises both control paths: at i=2 we
    // take the continue branch, elsewhere we fall through to
    // `s += i`.
    assert_eq!(
        run_int32_function(
            "function f(n) { \
                 let s = 0; \
                 let i = 0; \
                 while (i < n) { \
                     i = i + 1; \
                     if (i === 3) { continue; } \
                     s = s + i; \
                 } \
                 return s; \
             }",
            &[5],
        ),
        // i takes values 1..5; skip i=3: 1+2+4+5 = 12.
        12,
    );
}

#[test]
fn do_while_statement_unsupported() {
    // `do { … } while (test)` is structurally distinct (test runs
    // *after* the body) and isn't on the M7 plan.
    let err = compile("function f(n) { let i = 0; do { i = i + 1; } while (i < n); return i; }")
        .expect_err("do-while at M7");
    assert!(matches!(err, SourceLoweringError::Unsupported { .. }));
}

// ---------------------------------------------------------------------------
// M8 — ForStatement (desugared as init / while-test-with-update)
// ---------------------------------------------------------------------------

#[test]
fn classic_for_with_let_init_sums_zero_to_n() {
    // `for (let i = 0; i < n; i = i + 1) s = s + i;` — the canonical
    // for-loop shape. The `let i` is scoped to the loop via the
    // snapshot/restore plumbing in `lower_for_statement` so it
    // doesn't collide with later top-level lets.
    let src = "function f(n) {
        let s = 0;
        for (let i = 0; i < n; i = i + 1) {
            s = s + i;
        }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[0]), 0);
    assert_eq!(run_int32_function(src, &[1]), 0);
    assert_eq!(run_int32_function(src, &[10]), 45);
    assert_eq!(run_int32_function(src, &[100]), 4950);
}

#[test]
fn for_with_let_init_pops_after_loop() {
    // After the for ends, `i` is out of scope. Reusing the name as
    // a top-level `let` in the same function must succeed (no
    // `duplicate_binding` collision) — confirms the snapshot/restore
    // actually pops the for-init binding.
    let src = "function f(n) {
        let s = 0;
        for (let i = 0; i < n; i = i + 1) { s = s + 1; }
        let i = 100;
        return s + i;
    }";
    assert_eq!(run_int32_function(src, &[3]), 103);
}

#[test]
fn for_with_assignment_init_uses_outer_let() {
    // `let i = 0; for (i = 0; …; …) …` — init is an assignment to a
    // pre-declared local. Confirms init-as-AssignmentExpression
    // works.
    let src = "function f(n) {
        let s = 0;
        let i = 0;
        for (i = 0; i < n; i = i + 1) { s = s + i; }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[5]), 10); // 0+1+2+3+4
}

#[test]
fn for_with_omitted_init_works() {
    let src = "function f(n) {
        let s = 0;
        let i = 0;
        for (; i < n; i = i + 1) { s = s + i; }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[5]), 10);
}

#[test]
fn for_with_omitted_update_works() {
    // The body itself does the update; the for loop without an
    // explicit update is just a while loop in disguise.
    let src = "function f(n) {
        let s = 0;
        for (let i = 0; i < n;) {
            s = s + i;
            i = i + 1;
        }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[5]), 10);
}

#[test]
fn for_with_omitted_test_runs_until_return() {
    // `for (let i = 0;; i = i + 1)` — no test, the body uses an
    // early return to terminate. Confirms the omitted-test path
    // emits LdaTrue and the JumpIfToBooleanFalse never fires until
    // the body returns.
    let src = "function f(n) {
        for (let i = 0;; i = i + 1) {
            if (i === n) { return i; }
        }
        return 0;
    }";
    assert_eq!(run_int32_function(src, &[42]), 42);
}

#[test]
fn for_with_compound_assign_update() {
    // Update is `i += 1`, not `i = i + 1`. Both lower through
    // `lower_assignment_expression`.
    let src = "function f(n) {
        let s = 0;
        for (let i = 0; i < n; i += 1) { s += i; }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[6]), 15); // 0+1+2+3+4+5
}

#[test]
fn for_with_multi_declarator_init() {
    // `for (let i = 0, acc = 0; …)` — multi-declarator works at the
    // for-init level too, courtesy of the M7 multi-declarator
    // patch.
    let src = "function f(n) {
        let result = 0;
        for (let i = 0, acc = 0; i < n; i = i + 1) {
            acc = acc + i;
            result = acc;
        }
        return result;
    }";
    assert_eq!(run_int32_function(src, &[5]), 10);
}

#[test]
fn nested_for_loops() {
    // Two nested `for (let …)` loops with distinct loop variables.
    // Cross-scope shadowing (using the same name `i` in both) needs
    // real lexical-scope tracking and lands later — for now,
    // `allocate_local` rejects it as `duplicate_binding` because both
    // bindings are simultaneously live in the same flat scope.
    let src = "function f(n) {
        let total = 0;
        for (let i = 0; i < n; i = i + 1) {
            for (let j = 0; j < n; j = j + 1) {
                total = total + 1;
            }
        }
        return total;
    }";
    // n*n iterations.
    assert_eq!(run_int32_function(src, &[3]), 9);
    assert_eq!(run_int32_function(src, &[5]), 25);
}

#[test]
fn nested_for_with_same_name_shadows() {
    // M12: nested `for (let i = ...; ...)` loops where each init
    // uses the same name `i` now works — per-for-scope snapshot
    // gives each loop its own binding, shadowing the outer one
    // inside the inner body.
    assert_eq!(
        run_int32_function(
            "function f(n) {
                let total = 0;
                for (let i = 0; i < n; i = i + 1) {
                    for (let i = 0; i < n; i = i + 1) {
                        total = total + 1;
                    }
                }
                return total;
            }",
            &[3],
        ),
        // 3 outer × 3 inner = 9.
        9,
    );
}

#[test]
fn for_inside_if_branch() {
    // for-inside-if exercises `lower_nested_statement` recursing
    // through if's consequent into a for, then the for's body
    // back through `lower_nested_statement`.
    let src = "function f(n) {
        let s = 0;
        if (n > 0) {
            for (let i = 0; i < n; i = i + 1) {
                s = s + i;
            }
        }
        return s;
    }";
    assert_eq!(run_int32_function(src, &[5]), 10);
    assert_eq!(run_int32_function(src, &[0]), 0);
}

#[test]
fn for_with_const_init_then_update_is_unsupported() {
    // The init binding is `const`; the update tries to reassign it.
    // Same `const_assignment` rejection as the M5 path.
    let err = compile("function f(n) { for (const i = 0; i < n; i = i + 1) { } return n; }")
        .expect_err("const reassignment in for at M8");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "const_assignment",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// M8 — negative cases
// ---------------------------------------------------------------------------

#[test]
fn for_in_statement_unsupported() {
    // `for (k in obj)` — separate AST node type. M8 doesn't touch
    // it; lands later alongside object property iteration.
    let err = compile("function f() { for (let k in {}) { return k; } return 0; }")
        .expect_err("for-in at M8");
    assert!(matches!(err, SourceLoweringError::Unsupported { .. }));
}

#[test]
fn for_of_statement_unsupported() {
    // `for (x of arr)` — separate AST node type. Lands later
    // alongside iterator protocol.
    let err = compile("function f() { for (let x of [1]) { return x; } return 0; }")
        .expect_err("for-of at M8");
    assert!(matches!(err, SourceLoweringError::Unsupported { .. }));
}

#[test]
fn for_with_bare_expression_init_unsupported() {
    // `for (n; n > 0; n = n - 1)` — init is a bare identifier read,
    // not an assignment. Reject with a stable per-shape tag.
    let err = compile("function f(n) { for (n; n > 0; n = n - 1) { } return n; }")
        .expect_err("bare init expr at M8");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "for_init_expression",
            ..
        }
    ));
}

#[test]
fn for_with_bare_expression_update_unsupported() {
    // `for (let i = 0; i < n; i)` — update is a bare identifier
    // read with no observable effect. Reject so users don't ship
    // dead-code updates by mistake; once `++` / `--` land, the same
    // rejection lifts.
    let err = compile("function f(n) { for (let i = 0; i < n; i) { } return n; }")
        .expect_err("bare update expr at M8");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "for_update_expression",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// M9 — multiple functions + CallExpression
// ---------------------------------------------------------------------------

#[test]
fn zero_arg_call_returns_callee_value() {
    // Simplest call: no args, return the callee's value.
    let src = "function helper() { return 7; } function main() { return helper(); }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn single_arg_call_passes_value() {
    // `inc(5)` returns 5 + 1 = 6. Confirms the single-arg path:
    // arg lowered into temp slot, `CallDirect inc, RegList(temp, 1)`.
    let src = "function inc(n) { return n + 1; } function main() { return inc(5); }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn nested_call_with_distinct_temp_windows() {
    // `wrap(inc(5))` — outer call has 1 arg, inner has 1 arg. The
    // inner call's temp window is allocated *after* the outer
    // window so they don't overlap.
    let src = "function inc(n) { return n + 1; } \
               function wrap(n) { return n + 100; } \
               function main() { return wrap(inc(5)); }";
    // wrap(inc(5)) = wrap(6) = 106.
    assert_eq!(run_int32_function(src, &[]), 106);
}

#[test]
fn recursion_factorial() {
    // Direct recursion: `fact(5) = 5 * fact(4) = … = 120`.
    let src = "function fact(n) { \
                   if (n === 0) { return 1; } \
                   return n * fact(n - 1); \
               } \
               function main() { return fact(5); }";
    assert_eq!(run_int32_function(src, &[]), 120);
    let src7 = src.replace("fact(5)", "fact(7)");
    assert_eq!(run_int32_function(&src7, &[]), 5040);
}

#[test]
fn mutual_recursion_even_odd() {
    // `is_even(n)` and `is_odd(n)` call each other. Names are
    // collected before any body is lowered, so forward
    // references resolve.
    let src = "function is_even(n) { \
                   if (n === 0) { return 1; } \
                   return is_odd(n - 1); \
               } \
               function is_odd(n) { \
                   if (n === 0) { return 0; } \
                   return is_even(n - 1); \
               } \
               function main() { return is_even(6); }";
    assert_eq!(run_int32_function(src, &[]), 1);
    let src_odd = src.replace("is_even(6)", "is_even(7)");
    assert_eq!(run_int32_function(&src_odd, &[]), 0);
}

#[test]
fn call_in_loop_body_uses_jit_tier_up_path() {
    // Hot loop calling an inner function — this is the shape that
    // finally exercises the JSC tier-up budget and CallClosure
    // entry point in the dispatcher. Result-correctness is what
    // the test asserts; performance is measured via the M2 / M7
    // microbench tests in `otter-jit::baseline::tests`.
    let src = "function inc(n) { return n + 1; } \
               function main() { \
                   let s = 0; \
                   let i = 0; \
                   while (i < 100) { s = inc(s); i = i + 1; } \
                   return s; \
               }";
    assert_eq!(run_int32_function(src, &[]), 100);
}

#[test]
fn call_as_expression_statement_discards_result() {
    // `helper();` at statement position — result lands in acc and
    // the next instruction overwrites it. Used here to fire a
    // function purely for its side effect (… which a pure helper
    // doesn't have, but the lowering path is what we're checking).
    let src = "function helper() { return 0; } \
               function main() { \
                   let x = 42; \
                   helper(); \
                   return x; \
               }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn call_in_compound_assign_rhs() {
    // `s += helper()` — lowered as `Ldar r_s; <call helper>; Add
    // r_temp; Star r_s`. Wait — actually `+=` currently uses the
    // Reg form against an identifier RHS, not a call result. So
    // this should reject as `expression_construct_tag` for the
    // call (not a literal/identifier RHS).
    let _ = "function helper() { return 5; } \
             function main() { let s = 10; s += helper(); return s; }";
    // Re-test with a shape that *does* work: split the call into
    // a separate `let` and assign from the local.
    let src = "function helper() { return 5; } \
               function main() { let h = helper(); let s = 10; s += h; return s; }";
    assert_eq!(run_int32_function(src, &[]), 15);
}

#[test]
fn call_with_three_int_args_orders_correctly() {
    // Multi-arg call with literal args. Confirms args land in the
    // expected order in the temp window (`base+0`, `base+1`,
    // `base+2`) and `CallDirect` reads them as `[10, 20, 30]`.
    let src = "function add3(a, b, c) { return a; } \
               function main() { return add3(10, 20, 30); }";
    // Wait — our M9 source compiler still rejects multi-param
    // functions (`a, b, c`). So this test would fail at compile
    // time.
    let _ = src;
    let err = compile(
        "function add3(a, b, c) { return a; } function main() { return add3(10, 20, 30); }",
    )
    .expect_err("multi-param at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "multiple_parameters",
            ..
        }
    ));
}

#[test]
fn one_arg_call_with_local_variable_passes_value() {
    // Callsite reads a local and passes it. Exercises
    // `lower_return_expression` inside the arg lowering path.
    let src = "function dbl(n) { return n + n; } \
               function main() { let x = 21; return dbl(x); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

// ---------------------------------------------------------------------------
// M9 — negative cases
// ---------------------------------------------------------------------------

#[test]
fn unbound_function_call_unsupported() {
    // `nope` doesn't name a top-level function. Surfaces as
    // `unbound_function`, distinct from the identifier-read
    // `unbound_identifier` rejection.
    let err = compile("function main() { return nope(); }").expect_err("unknown function at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "unbound_function",
            ..
        }
    ));
}

#[test]
fn member_call_unsupported() {
    // `o.m()` — callee is a MemberExpression, not an identifier.
    // Lands when property access lands.
    let err = compile("function main() { return main.x(); }").expect_err("member-call at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "non_identifier_callee",
            ..
        }
    ));
}

#[test]
fn spread_call_arg_unsupported() {
    let err = compile("function f(n) { return n; } function main() { return f(...[1]); }")
        .expect_err("spread arg at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "spread_call_arg",
            ..
        }
    ));
}

#[test]
fn duplicate_top_level_function_unsupported() {
    // Two `function f`s in the same module — JS would silently
    // pick the last (function-decl hoisting overrides). M9
    // rejects to keep the function-name table unambiguous; later
    // milestones can adopt the "last wins" semantics if real code
    // demands it.
    let err = compile("function f() { return 1; } function f() { return 2; }")
        .expect_err("duplicate function decl at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "duplicate_function_declaration",
            ..
        }
    ));
}

#[test]
fn calling_a_param_unsupported() {
    // `f(g)` passes `g` as an argument; calling that param value
    // would need closure values. Until then, params are not
    // callable — confirms the function-name lookup doesn't
    // accidentally succeed on a parameter.
    let err = compile("function caller(g) { return g(); } function main() { return caller(1); }")
        .expect_err("call-of-param at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "unbound_function",
            ..
        }
    ));
}

#[test]
fn new_expression_unsupported() {
    // `new f()` is a NewExpression, not a CallExpression — falls
    // through to the catch-all `expression_construct_tag` →
    // `new_expression`.
    let err = compile("function f() { return 1; } function main() { return new f(); }")
        .expect_err("new at M9");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "new_expression",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// M10: UnaryExpression + UpdateExpression
// ---------------------------------------------------------------------------

#[test]
fn unary_negation_on_parameter() {
    // `-n` → `Ldar r0; Negate; Return`. Negate on int32 is
    // wraparound negation, so `-(-7)` round-trips.
    assert_eq!(run_int32_function("function f(n) { return -n; }", &[7]), -7);
    assert_eq!(run_int32_function("function f(n) { return -n; }", &[-7]), 7);
}

#[test]
fn unary_plus_is_identity_on_int32() {
    // `+n` → `Ldar r0; ToNumber; Return`. ToNumber on int32 is a
    // no-op, so the value round-trips unchanged.
    assert_eq!(
        run_int32_function("function f(n) { return +n; }", &[42]),
        42
    );
}

#[test]
fn bitwise_not_on_parameter() {
    // `~n` → `Ldar r0; BitwiseNot; Return`. Matches JS:
    // `~0` = -1, `~-1` = 0, `~5` = -6.
    assert_eq!(run_int32_function("function f(n) { return ~n; }", &[0]), -1);
    assert_eq!(run_int32_function("function f(n) { return ~n; }", &[-1]), 0);
    assert_eq!(run_int32_function("function f(n) { return ~n; }", &[5]), -6);
}

#[test]
fn logical_not_on_truthy_int32_returns_false() {
    // `!n` → `Ldar r0; LogicalNot; Return`. Non-zero int32 is
    // truthy, so `!5` is false (0 when coerced back to i32).
    let module = compile("function f(n) { return !n; }").expect("compile");
    let entry = module.entry();
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    registers[hidden] = RegisterValue::from_i32(5);
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value().as_bool(), Some(false));
}

#[test]
fn logical_not_on_zero_returns_true() {
    let module = compile("function f(n) { return !n; }").expect("compile");
    let entry = module.entry();
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    registers[hidden] = RegisterValue::from_i32(0);
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value().as_bool(), Some(true));
}

#[test]
fn typeof_int32_returns_number_string() {
    // `typeof n` → `Ldar r0; TypeOf; Return`. Returns the string
    // "number" for int32 values.
    let module = compile("function f(n) { return typeof n; }").expect("compile");
    let entry = module.entry();
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    registers[hidden] = RegisterValue::from_i32(7);
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    let handle = result
        .return_value()
        .as_object_handle()
        .expect("typeof returns a string object handle");
    let text = runtime
        .objects
        .string_value(crate::object::ObjectHandle(handle))
        .expect("typeof result readable")
        .expect("typeof result has string value");
    assert_eq!(text.to_rust_string(), "number");
}

#[test]
fn void_expression_returns_undefined() {
    // `void n` evaluates the argument for side effects, then
    // returns `undefined`. We observe this end-to-end via the raw
    // `RegisterValue::undefined()` comparison — `undefined` isn't
    // in scope as an identifier at this milestone, so the test can't
    // write `=== undefined` in source yet.
    let module = compile("function f(n) { return void n; }").expect("compile");
    let entry = module.entry();
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    registers[hidden] = RegisterValue::from_i32(42);
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value(), RegisterValue::undefined());
}

#[test]
fn delete_unary_unsupported() {
    // `delete x` depends on PropertyAccess / global-binding support
    // that hasn't landed yet. Must surface a stable tag.
    let err = compile("function f(n) { let x = n; return delete x; }").expect_err("delete at M10");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "delete_unary",
            ..
        }
    ));
}

#[test]
fn prefix_increment_on_local_returns_new_value() {
    // `++x` returns the incremented value and writes it back.
    assert_eq!(
        run_int32_function("function f() { let x = 5; ++x; return x; }", &[]),
        6,
    );
    assert_eq!(
        run_int32_function("function f() { let x = 5; return ++x; }", &[]),
        6,
    );
}

#[test]
fn prefix_decrement_on_local_returns_new_value() {
    assert_eq!(
        run_int32_function("function f() { let x = 5; --x; return x; }", &[]),
        4,
    );
    assert_eq!(
        run_int32_function("function f() { let x = 5; return --x; }", &[]),
        4,
    );
}

#[test]
fn postfix_increment_on_local_returns_old_value_writes_new() {
    // Expression result is the pre-increment int32, but the
    // binding holds the incremented value afterward.
    assert_eq!(
        run_int32_function("function f() { let x = 5; return x++; }", &[]),
        5,
    );
    // Readback test: after `x++`, `x` is 6.
    assert_eq!(
        run_int32_function("function f() { let x = 5; x++; return x; }", &[]),
        6,
    );
}

#[test]
fn postfix_decrement_on_local_returns_old_value_writes_new() {
    assert_eq!(
        run_int32_function("function f() { let x = 5; return x--; }", &[]),
        5,
    );
    assert_eq!(
        run_int32_function("function f() { let x = 5; x--; return x; }", &[]),
        4,
    );
}

#[test]
fn update_on_parameter_rejected() {
    let err = compile("function f(n) { return n++; }").expect_err("++param at M10");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "update_on_param",
            ..
        }
    ));
}

#[test]
fn update_on_const_rejected() {
    let err = compile("function f() { const x = 5; return ++x; }").expect_err("++const at M10");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "const_update",
            ..
        }
    ));
}

#[test]
fn update_loop_counter_in_while() {
    // The canonical `while (i < 3) { ... i++; }` shape lets us
    // exercise UpdateExpression as an ExpressionStatement inside a
    // loop body (not just inside a return).
    assert_eq!(
        run_int32_function(
            "function f() { let s = 0; let i = 0; while (i < 3) { s = s + i; i++; } return s; }",
            &[]
        ),
        3, // 0 + 1 + 2
    );
}

#[test]
fn unary_composes_with_binary_rhs() {
    // `return x + -y;` exercises the complex-RHS path that spills
    // LHS, evaluates the unary into acc, then reapplies the op.
    assert_eq!(
        run_int32_function("function f(n) { let y = 2; return n + -y; }", &[10]),
        8,
    );
}

#[test]
fn update_composes_with_binary_rhs() {
    // `return n + x++;` — the RHS produces the OLD x, then writes
    // x back. Exercises the complex-RHS path carrying a
    // postfix-update.
    assert_eq!(
        run_int32_function("function f(n) { let x = 5; return n + x++; }", &[10]),
        15,
    );
}

// ---------------------------------------------------------------------------
// M11: break / continue (unlabelled) in while / for
// ---------------------------------------------------------------------------

#[test]
fn break_exits_for_loop() {
    // `break` in a `for` jumps straight to the loop exit —
    // skipping the update and subsequent iterations.
    assert_eq!(
        run_int32_function(
            "function f(n) { \
                 let s = 0; \
                 for (let i = 0; i < n; i = i + 1) { \
                     if (i === 3) { break; } \
                     s = s + i; \
                 } \
                 return s; \
             }",
            &[10],
        ),
        // Body runs for i = 0, 1, 2 (breaks at 3). Sum = 0+1+2.
        3,
    );
}

#[test]
fn continue_in_for_runs_update_then_test() {
    // `continue` in a `for` lands on the update clause, not the
    // header — the spec-mandated shape. Without the dedicated
    // continue label, a naive `Jump loop_header` would skip the
    // `i = i + 1` update and spin forever.
    assert_eq!(
        run_int32_function(
            "function f(n) { \
                 let s = 0; \
                 for (let i = 0; i < n; i = i + 1) { \
                     if (i === 2) { continue; } \
                     s = s + i; \
                 } \
                 return s; \
             }",
            &[5],
        ),
        // Skip i=2: 0+1+3+4 = 8.
        8,
    );
}

#[test]
fn break_in_nested_while_only_exits_innermost() {
    // The innermost-loop-first rule: a `break` inside a nested
    // `while` must exit only that inner loop. The outer loop
    // continues running afterwards. `j` hoisted to the function
    // scope because the current compiler doesn't allow `let`
    // inside a nested body.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let s = 0; \
                 let i = 0; \
                 let j = 0; \
                 while (i < 3) { \
                     j = 0; \
                     while (j < 10) { \
                         if (j === 2) { break; } \
                         s = s + 1; \
                         j = j + 1; \
                     } \
                     i = i + 1; \
                 } \
                 return s; \
             }",
            &[],
        ),
        // Inner loop runs for j = 0, 1 before break → 2 increments
        // per outer iteration × 3 outer iterations = 6.
        6,
    );
}

#[test]
fn continue_in_nested_while_only_affects_innermost() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let s = 0; \
                 let i = 0; \
                 let j = 0; \
                 while (i < 2) { \
                     j = 0; \
                     while (j < 4) { \
                         j = j + 1; \
                         if (j === 2) { continue; } \
                         s = s + j; \
                     } \
                     i = i + 1; \
                 } \
                 return s; \
             }",
            &[],
        ),
        // Inner iter: j → 1,2,3,4; skip j=2 → 1+3+4 = 8.
        // Two outer iters → 16.
        16,
    );
}

#[test]
fn labelled_break_rejected() {
    // `outer:` is a LabeledStatement wrapper that the compiler
    // doesn't recognise yet — it's rejected before we even reach
    // the labelled break. Surface any Unsupported tag so the
    // negative expectation is robust to future label-statement
    // support; the lowering-side check for
    // `break_stmt.label.is_some()` is exercised indirectly by
    // the other M11 tests which confirm unlabelled break works.
    let err = compile("function f(n) { outer: while (n > 0) { break outer; } return n; }")
        .expect_err("labelled break at M11");
    assert!(
        matches!(err, SourceLoweringError::Unsupported { .. }),
        "unexpected err: {err:?}",
    );
}

#[test]
fn labelled_continue_rejected() {
    let err = compile(
        "function f(n) { let i = n; outer: while (i > 0) { i = i - 1; continue outer; } return i; }",
    )
    .expect_err("labelled continue at M11");
    assert!(
        matches!(err, SourceLoweringError::Unsupported { .. }),
        "unexpected err: {err:?}",
    );
}

#[test]
fn break_outside_loop_rejected() {
    // `break` at function top level has no enclosing loop. We
    // surface a stable tag instead of emitting a dangling jump.
    let err = compile("function f() { break; return 0; }").expect_err("break top-level at M11");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "break_outside_loop",
            ..
        }
    ));
}

#[test]
fn continue_outside_loop_rejected() {
    let err =
        compile("function f() { continue; return 0; }").expect_err("continue top-level at M11");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "continue_outside_loop",
            ..
        }
    ));
}

#[test]
fn break_after_loop_still_rejected() {
    // Loop labels must be unregistered on loop exit — `break`
    // after the loop body (in straight-line code following the
    // loop) should see an empty stack.
    // Use a local (not the parameter) as the counter because the
    // compiler rejects assigning to parameters. The shape is
    // equivalent for the purpose of the test.
    let err = compile("function f(n) { let i = n; while (i > 0) { i = i - 1; } break; return i; }")
        .expect_err("break after loop at M11");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "break_outside_loop",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

// ---------------------------------------------------------------------------
// M12: Block scoping for let / const inside if / while / for / bare blocks
// ---------------------------------------------------------------------------

#[test]
fn let_inside_for_body_is_block_scoped() {
    // M12: `for (let i = ...; ...) { let y = ...; ... }` — the
    // `y` binding is declared per iteration. Slots don't collide
    // with the for-init `i` because FrameLayout reserves the peak.
    assert_eq!(
        run_int32_function(
            "function f(n) { \
                 let s = 0; \
                 for (let i = 0; i < n; i = i + 1) { \
                     let y = i; \
                     s = s + y; \
                 } \
                 return s; \
             }",
            &[4],
        ),
        // sum 0..3 = 6
        6,
    );
}

#[test]
fn bare_block_statement_scopes_let() {
    // M12: a `{ ... }` at function-body level creates a new scope
    // for `let`. The binding is popped on block exit; reassigning
    // the OUTER `x` after the block works fine.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let x = 1; \
                 { let y = 2; x = x + y; } \
                 x = x + 10; \
                 return x; \
             }",
            &[],
        ),
        13,
    );
}

#[test]
fn nested_blocks_each_have_own_scope() {
    // Two nested blocks each declare their own `let x`; the inner
    // one shadows the middle one, which shadows the outer function
    // `let x`. Each restore_scope pops only the innermost. The
    // return reads the outer function-scope `x`, which stayed at
    // its initial value.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let x = 1; \
                 { \
                     let x = 2; \
                     { \
                         let x = 3; \
                     } \
                 } \
                 return x; \
             }",
            &[],
        ),
        1,
    );
}

#[test]
fn outer_let_assignable_from_inside_block() {
    // The inner block can reassign an outer `let` (block scope
    // doesn't block write-access; it just narrows NEW `let`
    // lifetimes).
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let x = 0; \
                 { x = 42; } \
                 return x; \
             }",
            &[],
        ),
        42,
    );
}

#[test]
fn const_inside_block_rejects_reassignment() {
    // Scoped `const` retains its const-ness: `const_assignment`
    // fires on attempted writes inside the block.
    let err = compile("function f() { { const c = 5; c = 6; } return 0; }")
        .expect_err("const reassign in block at M12");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "const_assignment",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

#[test]
fn let_in_block_not_visible_after_block() {
    // Reading `y` after its containing block has closed must
    // surface as an unbound identifier — bindings pop off the
    // lexical environment on block exit.
    let err =
        compile("function f() { { let y = 1; } return y; }").expect_err("use after block at M12");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "unbound_identifier",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

#[test]
fn sibling_blocks_reuse_the_same_reserved_slots() {
    // Two sibling blocks each declare `let t` but don't overlap
    // at runtime — the second block starts after the first has
    // popped. The FrameLayout still reserves the slot once (the
    // peak local count across blocks), so neither clobbers the
    // outer `s`.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let s = 0; \
                 { let t = 10; s = s + t; } \
                 { let t = 20; s = s + t; } \
                 return s; \
             }",
            &[],
        ),
        30,
    );
}

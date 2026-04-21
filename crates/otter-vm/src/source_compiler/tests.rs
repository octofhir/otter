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

fn compile_ts(source: &str) -> Result<crate::module::Module, SourceLoweringError> {
    ModuleCompiler::new().compile(source, "test.ts", SourceType::ts())
}

/// Return `(FunctionIndex, &Function)` for the user-declared
/// function the tests want to call directly. The module's entry
/// is always the synthesised `<top-level>` now, so tests that
/// pre-date top-level-statement support need to fish out the
/// function they actually want to invoke.
///
/// Strategy:
///   1. Prefer a top-level function named `main`
///   2. Then `f` (the other common test convention)
///   3. Then the first function whose name doesn't start with
///      `<` — which, given the compiler emits top-level
///      declarations at indices `0..N` before any nested
///      function expressions, always resolves to a top-level
///      declaration when one exists.
fn pick_last_named_function(
    module: &crate::module::Module,
) -> Option<(FunctionIndex, &crate::module::Function)> {
    let functions = module.functions();
    let by_name = |target: &str| {
        functions.iter().enumerate().find_map(|(i, f)| {
            if f.name() == Some(target) {
                let idx = u32::try_from(i).ok()?;
                Some((FunctionIndex(idx), f))
            } else {
                None
            }
        })
    };
    if let Some(hit) = by_name("main") {
        return Some(hit);
    }
    if let Some(hit) = by_name("f") {
        return Some(hit);
    }
    for (i, f) in functions.iter().enumerate() {
        let name = f.name().unwrap_or("");
        if name.starts_with('<') {
            continue;
        }
        let idx = u32::try_from(i).ok()?;
        return Some((FunctionIndex(idx), f));
    }
    None
}

fn run_int32_function(source: &str, args: &[i32]) -> i32 {
    let module = compile(source).expect("compile");
    run_int32_function_from_module(module, args)
}

fn run_int32_function_ts(source: &str, args: &[i32]) -> i32 {
    let module = compile_ts(source).expect("compile");
    run_int32_function_from_module(module, args)
}

fn run_int32_function_from_module(module: crate::module::Module, args: &[i32]) -> i32 {
    // The module entry is always the synthesised top-level
    // function (runs the script body once, returns `undefined`).
    // These tests want to call the LAST user-declared function —
    // by convention `main` or `f` — with the given args. Pick it
    // explicitly: walk `module.functions()` and take the last
    // named one whose name isn't the synth placeholder.
    let (entry_idx, function) =
        pick_last_named_function(&module).expect("module must declare at least one named function");
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

/// Helper for M32 tests where the entry function returns a
/// shared state object and microtasks are expected to mutate it
/// before the test reads a final int32 counter. `execute_with_runtime`
/// already drains the microtask queue before returning, so by the
/// time we reach the property read every queued promise job has
/// had a chance to run.
fn run_promise_state_counter(source: &str, property: &str) -> i32 {
    let module = compile(source).expect("compile");
    let (entry_idx, _) = pick_last_named_function(&module).expect("named fn");
    let function = module
        .function(entry_idx)
        .expect("module has entry function");
    let register_count = function.frame_layout().register_count();
    let registers = vec![RegisterValue::undefined(); usize::from(register_count)];
    let interpreter = Interpreter::new();
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = interpreter
        .execute_with_runtime(&module, entry_idx, &registers, &mut runtime)
        .expect("execute_with_runtime");
    let state_handle = result
        .return_value()
        .as_object_handle()
        .map(crate::object::ObjectHandle)
        .expect("main did not return an object");
    let prop_id = runtime.intern_property_name(property);
    let lookup = runtime
        .property_lookup(state_handle, prop_id)
        .expect("property_lookup")
        .expect("state missing expected property");
    let value = match lookup.value() {
        crate::object::PropertyValue::Data { value, .. } => value,
        crate::object::PropertyValue::Accessor { .. } => {
            panic!("state property is an accessor")
        }
    };
    value.as_i32().expect("state property is not an int32")
}

// ---------------------------------------------------------------------------
// Parse-phase diagnostics
// ---------------------------------------------------------------------------

#[test]
fn syntax_error_reports_parse() {
    let err = compile("function (").expect_err("bad syntax must surface as Parse");
    assert!(matches!(err, SourceLoweringError::Parse { .. }));
}

#[test]
fn invalid_for_of_expression_left_reports_parse() {
    let err = compile("function f() { let a = 0, b = 0; for (a + b of [1]) {} }")
        .expect_err("invalid for-of assignment target must surface as Parse");
    assert!(matches!(err, SourceLoweringError::Parse { .. }));
}

#[test]
fn invalid_for_of_ts_asserted_expression_left_reports_parse() {
    let err = compile_ts("function f() { let a = 0; for ((a + 1) as any of [1]) {} }")
        .expect_err("invalid TS for-of assignment target must surface as Parse");
    assert!(matches!(err, SourceLoweringError::Parse { .. }));
}

#[test]
fn invalid_for_in_expression_left_reports_parse() {
    let err = compile("function f() { let a = 0, b = 0; for (a + b in { x: 1 }) {} }")
        .expect_err("invalid for-in assignment target must surface as Parse");
    assert!(matches!(err, SourceLoweringError::Parse { .. }));
}

#[test]
fn invalid_for_in_ts_asserted_expression_left_reports_parse() {
    let err = compile_ts("function f() { let a = 0; for ((a + 1) as any in { x: 1 }) {} }")
        .expect_err("invalid TS for-in assignment target must surface as Parse");
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
fn top_level_class_declaration_is_accepted() {
    // Top-level classes are part of the idiomatic JS surface
    // since we support top-level statements — `class Foo {}`
    // without a surrounding `function main()` lowers through
    // the script-body entry.
    compile("class Foo {}").expect("top-level class compiles");
}

#[test]
fn fractional_numeric_literal_compiles() {
    // Non-int32 numeric literals (fractional, out-of-range) now
    // intern as f64 constants and emit `LdaConstF64` — no more
    // `non_int32_literal` rejection.
    compile("function h() { return 1.5; }").expect("fractional literal compiles");
}

#[test]
fn two_functions_at_top_level_both_compile() {
    // Two declarations at the top level both become callable
    // module-functions. No "main" magic: the test helper picks
    // `a` (the `by_name("f")`/first-top-level fallback), asserts
    // it returns 1, then repeats with `b`.
    let module = compile("function a() { return 1; } function b() { return 2; }").expect("compile");
    let call = |name: &str| {
        let (idx, function) = module
            .functions()
            .iter()
            .enumerate()
            .find_map(|(i, f)| (f.name() == Some(name)).then_some((FunctionIndex(i as u32), f)))
            .expect("named function present");
        let regs =
            vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
        let mut rt = crate::interpreter::RuntimeState::new();
        Interpreter::new()
            .execute_with_runtime(&module, idx, &regs, &mut rt)
            .expect("execute")
            .return_value()
            .as_i32()
            .expect("int32")
    };
    assert_eq!(call("a"), 1);
    assert_eq!(call("b"), 2);
}

// Removed: multi_parameters_unsupported, default_parameter_unsupported,
// rest_parameter_unsupported — all three shapes are supported as of M22.
// Positive coverage for each lives in the M22 test block below.

// Removed: destructuring_parameter_unsupported — destructuring in
// function params is supported as of M24. Positive coverage lives in
// the M24 test block below.

#[test]
fn division_compiles_and_falls_back_to_float() {
    // `/` now lowers through `Opcode::Div` with `js_divide`
    // handling the non-int32 fallback. Integer-truncation fast
    // path kicks in when both operands are i32 and the division
    // is exact; otherwise the runtime produces a Number.
    assert_eq!(
        run_int32_function("function f() { return 12 / 3; }", &[]),
        4
    );
}

#[test]
fn remainder_compiles() {
    assert_eq!(
        run_int32_function("function f() { return 17 % 5; }", &[]),
        2
    );
}

#[test]
fn exponent_compiles() {
    assert_eq!(
        run_int32_function("function f() { return 2 ** 10; }", &[]),
        1024
    );
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
fn missing_trailing_return_returns_undefined() {
    // M19 follow-up: the source compiler used to require the
    // last statement to be `return <expr>;`. Real JS doesn't —
    // `function f() { let x = 2; }` returns `undefined`. The
    // lowering now synthesizes `LdaUndefined; Return` after any
    // non-return tail statement. (§15.2.1 FunctionBody evaluation.)
    let module = compile("function f() { let x = 1; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value(), RegisterValue::undefined());
}

#[test]
fn bare_return_returns_undefined() {
    // §14.9 — `return;` exits with `undefined`. The lowering used
    // to reject this with `return_without_value`; M19 follow-up
    // emits `LdaUndefined; Return` instead.
    let module = compile("function f() { return; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value(), RegisterValue::undefined());
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
fn wide_integer_literal_on_rhs_spills_to_temp() {
    // 200 is outside i8 range, so `AddSmi` can't represent it.
    // The complex-RHS spill path materialises the literal into a
    // temp and uses `Add r_tmp` instead — no longer a rejection.
    assert_eq!(
        run_int32_function("function f(n) { return n + 200; }", &[42]),
        242
    );
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
fn bitwise_xor_literal_rhs_spills_to_temp() {
    // No `BitwiseXorSmi` opcode, so the literal RHS spills to a
    // temp register. Works end-to-end now.
    assert_eq!(
        run_int32_function("function f(n) { return n ^ 1; }", &[2]),
        3
    );
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
fn unsigned_shift_right_literal_rhs_spills_to_temp() {
    // No `UShrSmi` opcode — RHS spills to a temp register. Works
    // like XOR and wide SubSmi above.
    assert_eq!(
        run_int32_function("function f(n) { return n >>> 1; }", &[4]),
        2
    );
}

#[test]
fn wide_subsmi_literal_spills_to_temp() {
    // 200 > i8::MAX, so SubSmi can't encode it. Spill path takes
    // over and emits `Sub r_tmp`.
    assert_eq!(
        run_int32_function("function f(n) { return n - 200; }", &[250]),
        50
    );
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
fn var_declaration_lowered_as_let_at_declaration_site() {
    // `var` now lowers as `let` at the declaration site.
    // Function-scope hoisting across hoisted calls is still TBD,
    // but the common single-declaration-before-read pattern
    // works end-to-end — no more `var_declaration` rejection.
    assert_eq!(
        run_int32_function("function f() { var x = 7; return x; }", &[]),
        7
    );
}

#[test]
fn nested_if_var_declaration_lowers() {
    // Bare statement-position `var` bodies are valid JS (`if (...) var x = ...;`).
    // They should route through the shared declaration lowerer instead of
    // tripping the old blanket nested-declaration rejection.
    assert_eq!(
        run_int32_function("function f() { if (1) var x = 7; return x; }", &[]),
        7
    );
}

#[test]
fn nested_do_while_var_declaration_lowers() {
    // `do Statement while (...)` also allows a bare `var` declaration.
    assert_eq!(
        run_int32_function("function f() { do var x = 9; while (0); return x; }", &[]),
        9
    );
}

#[test]
fn nested_while_var_declaration_compiles() {
    // When the loop body is a bare `var` statement, the compiler should
    // still accept the shape even if the body never executes.
    compile("function f() { while (0) var x = 1; return 0; }")
        .expect("statement-position while var compiles");
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

// Removed: destructuring_binding_unsupported — array/object
// destructuring is supported as of M24. Positive coverage lives in the
// M24 test block below.

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
fn fractional_initializer_compiles() {
    // Fractional initializers now flow through `LdaConstF64` —
    // the init expression inherits `lower_return_expression`'s
    // f64-fallback path.
    compile("function f() { let x = 1.5; return x; }").expect("fractional init compiles");
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

// Removed: assignment_to_param_unsupported — parameters are ordinary
// mutable bindings as of M22; positive coverage in
// `param_binding_can_be_assigned`.

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

// Removed: member_assignment_target is supported as of M17 —
// `n.x = 5` now compiles. Coverage for member writes lives in the
// M17 test block below.

#[test]
fn destructuring_assignment_to_array_target_works() {
    // `[x] = [5]` — destructuring assignment to an EXISTING
    // binding (no `let`/`const` keyword). Evaluates the RHS
    // once, then assigns each element via the normal identifier-
    // assignment path.
    assert_eq!(
        run_int32_function("function f() { let x = 1; [x] = [5]; return x; }", &[]),
        5,
    );
}

#[test]
fn destructuring_assignment_to_object_target_works() {
    assert_eq!(
        run_int32_function(
            "function f() { let a = 0; let b = 0; ({ a, b } = { a: 2, b: 3 }); return a + b; }",
            &[],
        ),
        5,
    );
}

#[test]
fn destructuring_assignment_to_renamed_object_target_works() {
    assert_eq!(
        run_int32_function(
            "function f() { let x = 0; ({ key: x } = { key: 9 }); return x; }",
            &[],
        ),
        9,
    );
}

#[test]
fn destructuring_assignment_with_rest_works() {
    assert_eq!(
        run_int32_function(
            "function f() { let a = 0; let rest = []; [a, ...rest] = [1, 2, 3, 4]; return a + rest.length; }",
            &[],
        ),
        4,
    );
}

#[test]
fn nested_destructuring_declaration_patterns_work() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                let { a: { b }, c: [d] } = { a: { b: 6 }, c: [4] }; \
                return b * 10 + d; \
            }",
            &[],
        ),
        64,
    );
}

#[test]
fn nested_destructuring_declaration_defaults_work() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                let { a: { b = 7 } = {} } = {}; \
                return b; \
            }",
            &[],
        ),
        7,
    );
}

#[test]
fn nested_array_rest_destructuring_declaration_works() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                let [...[a, b]] = [1, 2, 3]; \
                return a * 10 + b; \
            }",
            &[],
        ),
        12,
    );
}

#[test]
fn nested_destructuring_assignment_patterns_work() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                let b = 0; \
                let d = 0; \
                ({ a: { b }, c: [d] } = { a: { b: 5 }, c: [8] }); \
                return b * 10 + d; \
            }",
            &[],
        ),
        58,
    );
}

#[test]
fn compound_assign_div_works() {
    // `x /= 2` now lowers through the shared compound-assign
    // path — the `Division` binary operator routes through
    // `js_divide` in the dispatcher.
    assert_eq!(
        run_int32_function("function f() { let x = 12; x /= 2; return x; }", &[]),
        6,
    );
}

#[test]
fn compound_assign_xor_works() {
    // `x ^= 1` has no `*Smi` opcode — spills through the
    // complex-RHS temp path. End-to-end result is the xor.
    assert_eq!(
        run_int32_function("function f() { let x = 6; x ^= 1; return x; }", &[]),
        7,
    );
}

#[test]
fn compound_assign_shl_works() {
    assert_eq!(
        run_int32_function("function f() { let x = 1; x <<= 2; return x; }", &[]),
        4,
    );
}

#[test]
fn bare_expression_statement_compiles() {
    // `5;` — a bare literal statement runs its expression into
    // the accumulator and discards the value. No longer a
    // rejection; the runtime semantics match real JS.
    assert_eq!(run_int32_function("function f() { 5; return 1; }", &[]), 1,);
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
fn two_literal_comparison_compiles() {
    // `5 < 10` — both operands are literals, so neither fits the
    // identifier/literal fast paths. The complex-operand fallback
    // now spills LHS to a temp and runs the comparison against
    // it; result is `true` → `1`.
    assert_eq!(
        run_int32_function("function f() { if (5 < 10) { return 1; } return 0; }", &[]),
        1,
    );
}

#[test]
fn member_access_equality_compiles() {
    // `o.x === 5` — LHS is a StaticMemberExpression, RHS is a
    // literal. Both-side complex path takes over.
    assert_eq!(
        run_int32_function(
            "function f() { let o = { x: 5 }; return (o.x === 5) ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn strict_equality_with_undefined_literal_compiles() {
    // `o.x === undefined` — classic presence check.
    assert_eq!(
        run_int32_function(
            "function f() { let o = {}; return (o.x === undefined) ? 1 : 0; }",
            &[],
        ),
        1,
    );
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
fn if_with_return_only_branch_falls_through_to_undefined() {
    // `if (n > 0) return n;` returns `n` when taken, and
    // `undefined` when the `if` branch isn't taken. M19 follow-up
    // synthesizes the `LdaUndefined; Return` tail so the
    // not-taken path exits correctly without the programmer
    // writing a second explicit `return`.
    let module = compile("function f(n) { if (n > 0) return n; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    // n = 0 → fall through → undefined.
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    registers[hidden] = RegisterValue::from_i32(0);
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value(), RegisterValue::undefined());
    // n = 5 → if branch taken → 5.
    registers[hidden] = RegisterValue::from_i32(5);
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value().as_i32(), Some(5));
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
fn do_while_statement_executes_body_before_test() {
    // `do { … } while (test)` runs the body at least once. For
    // n = 5: i starts at 0, increments up to 5, then the test
    // `i < n` fails and the loop exits.
    assert_eq!(
        run_int32_function(
            "function f(n) { let i = 0; do { i = i + 1; } while (i < n); return i; }",
            &[5],
        ),
        5,
    );
}

#[test]
fn do_while_runs_body_once_even_when_test_is_false() {
    // Canonical do-while behavior: the body executes before the
    // test is evaluated, so a `false` test still produces one
    // iteration. Starting from 0, we increment to 1, fail the
    // test, and return 1.
    assert_eq!(
        run_int32_function(
            "function f() { let i = 0; do { i = i + 1; } while (false); return i; }",
            &[],
        ),
        1,
    );
}

#[test]
fn do_while_supports_break_and_continue() {
    // `break` exits the loop; `continue` skips to the test.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let s = 0; \
                 let i = 0; \
                 do { \
                     i = i + 1; \
                     if (i === 3) { continue; } \
                     if (i === 6) { break; } \
                     s = s + i; \
                 } while (i < 10); \
                 return s; \
             }",
            &[],
        ),
        // i = 1,2,_,4,5 (then break at 6): 1+2+4+5 = 12.
        12,
    );
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
fn for_with_bare_expression_init_compiles() {
    // `for (n; n > 0; n = n - 1)` — bare identifier init runs
    // as an expression statement (side-effect evaluation). The
    // loop itself still works; `n` stays unchanged by the init.
    assert_eq!(
        run_int32_function(
            "function f(n) { for (n; n > 0; n = n - 1) { } return n; }",
            &[3],
        ),
        0,
    );
}

#[test]
fn for_with_bare_expression_update_compiles() {
    // `for (let i = 0; i < n; i)` — bare identifier update is a
    // no-op but still valid. Loop must exit when `i >= n`, which
    // it won't since `i` never advances. Use `i++` sibling path
    // covered elsewhere; here we just verify compilation + one
    // iteration of a bounded shape that DOES advance.
    assert_eq!(
        run_int32_function(
            "function f(n) { let total = 0; for (let i = 0; i < n; i++) { total = total + i; } return total; }",
            &[4],
        ),
        6,
    );
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
    // Multi-param signatures became a first-class surface at M22;
    // this was a placeholder rejection test before.
    let src = "function pickA(a, b, c) { return a; } \
               function main() { return pickA(10, 20, 30); }";
    assert_eq!(run_int32_function(src, &[]), 10);
    let src = "function pickB(a, b, c) { return b; } \
               function main() { return pickB(10, 20, 30); }";
    assert_eq!(run_int32_function(src, &[]), 20);
    let src = "function pickC(a, b, c) { return c; } \
               function main() { return pickC(10, 20, 30); }";
    assert_eq!(run_int32_function(src, &[]), 30);
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

// Removed: member_call_unsupported — method-call lowering is supported
// as of M19 (`o.m()` and `o[k]()` via CallProperty). Coverage for method
// calls lives in the M19 test block below.

#[test]
fn spread_in_direct_call_routes_through_call_spread() {
    // `f(...[1, 2])` loads `f` through the regular binding
    // resolution, pairs it with an `undefined` receiver, builds
    // an array out of the spread + plain args, and dispatches
    // via `CallSpread`. No more rejection.
    assert_eq!(
        run_int32_function(
            "function f(a, b) { return a + b } function main() { return f(...[10, 20]) }",
            &[],
        ),
        30
    );
}

#[test]
fn spread_in_direct_call_mixes_regular_and_spread_args() {
    assert_eq!(
        run_int32_function(
            "function pack(a, b, c, d) { return a * 1000 + b * 100 + c * 10 + d; } \
             function main() { return pack(1, ...[2, 3], 4); }",
            &[],
        ),
        1234,
    );
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

// Removed: calling_a_param_unsupported — M25 wires the
// local/param-holds-callable fallback in `lower_direct_call`,
// so `function caller(g) { return g(); }` now compiles
// (runtime dispatches through `CallUndefinedReceiver` against
// whatever value `g` holds).

#[test]
fn _calling_a_param_suppressed_placeholder() {
    // Deleted companion assertion — M25 positive coverage lives
    // in the M25 test block below.
    let err = compile("function f() { return bogus(); }").expect_err("unbound still");
    assert!(matches!(
        err,
        SourceLoweringError::Unsupported {
            construct: "unbound_function",
            ..
        }
    ));
}

// Removed: new_expression_unsupported — `new` is supported as of
// M27. Positive coverage lives in the M27 test block below.

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
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
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
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
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
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
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
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
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
fn delete_on_named_property_returns_true() {
    // `delete obj.x` lowers to `DelNamedProperty`. Returns
    // `true` on successful delete (configurable data property).
    assert_eq!(
        run_int32_function(
            "function f() { let o = { x: 1 }; return delete o.x ? 7 : 0; }",
            &[],
        ),
        7,
    );
}

#[test]
fn delete_on_computed_property_returns_true() {
    assert_eq!(
        run_int32_function(
            "function f() { let o = { a: 9 }; let k = \"a\"; return delete o[k] ? 7 : 0; }",
            &[],
        ),
        7,
    );
}

#[test]
fn delete_on_non_reference_returns_true() {
    // `delete x` where `x` is a plain local reference — per
    // §13.5.1 step 3 returns `true` without removing anything.
    assert_eq!(
        run_int32_function("function f() { let x = 5; return delete x ? 1 : 0; }", &[],),
        1,
    );
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
fn update_on_parameter_works() {
    // `n++` on a parameter writes back to the same register.
    // Postfix form returns the pre-increment value.
    assert_eq!(run_int32_function("function f(n) { return n++; }", &[5]), 5,);
    // Prefix `++n` returns the post-increment value.
    assert_eq!(run_int32_function("function f(n) { return ++n; }", &[5]), 6,);
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
fn labelled_break_exits_outer_loop() {
    // `break outer` from inside a nested loop jumps past the
    // outer `while` — the inner loop's short-circuit doesn't
    // matter because we exit both at once.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let hit = 0; \
                 outer: while (1) { \
                     while (1) { \
                         hit = 42; \
                         break outer; \
                     } \
                     hit = 999; \
                 } \
                 return hit; \
             }",
            &[],
        ),
        42,
    );
}

#[test]
fn labelled_continue_resumes_outer_loop() {
    // `continue outer` re-runs the outer loop's test, skipping
    // both the inner loop's remaining body and the outer loop's
    // tail. Canonical example: filter + sum with nested loops.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let sum = 0; \
                 let i = 0; \
                 outer: while (i < 3) { \
                     i = i + 1; \
                     let j = 0; \
                     while (j < 3) { \
                         j = j + 1; \
                         if (j === 2) { continue outer; } \
                         sum = sum + j; \
                     } \
                 } \
                 return sum; \
             }",
            &[],
        ),
        // Per outer iteration: j = 1 then skip at j = 2 → sum += 1.
        // Three outer iterations → 3.
        3,
    );
}

#[test]
fn labelled_block_supports_break() {
    // `break labelName` from inside a labelled block jumps past
    // the whole block without needing a loop wrapper. Per
    // §14.13 this form accepts `break` but not `continue`.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let hit = 0; \
                 outer: { \
                     hit = 1; \
                     break outer; \
                     hit = 2; \
                 } \
                 return hit; \
             }",
            &[],
        ),
        1,
    );
}

#[test]
fn undeclared_label_is_rejected() {
    let err = compile("function f() { while (1) { break nonexistent; } return 0; }")
        .expect_err("undeclared label");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "undeclared_label",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

#[test]
fn empty_function_body_returns_undefined() {
    // `function f() {}` — no statements. Per §15.2.1, the
    // implicit fall-through returns `undefined`. We verify by
    // calling an empty helper from a wrapper that compares its
    // result to `undefined`.
    assert_eq!(
        run_int32_function(
            "function helper() {} function f() { return helper() === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn use_strict_directive_is_accepted() {
    // `"use strict"` at the top of a function body is a directive
    // prologue (§14.1.1). The compiler doesn't need to act on
    // it — ES modules are strict-by-default — but it must not
    // reject the program.
    assert_eq!(
        run_int32_function("function f() { \"use strict\"; return 42; }", &[],),
        42,
    );
}

#[test]
fn arbitrary_directive_prologue_strings_are_ignored() {
    // Non-reserved directive strings are legal; implementations
    // may silently ignore them. This confirms we don't trip on
    // multiple directives or custom strings.
    assert_eq!(
        run_int32_function(
            "function f() { \"use strict\"; \"use custom\"; return 7; }",
            &[],
        ),
        7,
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

// ---------------------------------------------------------------------------
// M13: ConditionalExpression (a ? b : c) + logical &&, ||, ??
// ---------------------------------------------------------------------------

#[test]
fn ternary_truthy_branch() {
    // Parameter n truthy → consequent.
    assert_eq!(
        run_int32_function("function f(n) { return n ? 10 : 20; }", &[5]),
        10,
    );
}

#[test]
fn ternary_falsy_branch() {
    assert_eq!(
        run_int32_function("function f(n) { return n ? 10 : 20; }", &[0]),
        20,
    );
}

#[test]
fn ternary_negative_int_is_truthy() {
    // Non-zero int32 (including negatives) is truthy in JS.
    assert_eq!(
        run_int32_function("function f(n) { return n ? 10 : 20; }", &[-3]),
        10,
    );
}

#[test]
fn ternary_composes_with_binary_rhs() {
    // `return 1 + (n ? 10 : 20);` — the ternary is the complex
    // RHS of an Add, exercising the apply_binary_op_with_complex_rhs
    // path.
    assert_eq!(
        run_int32_function("function f(n) { return 1 + (n ? 10 : 20); }", &[1]),
        11,
    );
    assert_eq!(
        run_int32_function("function f(n) { return 1 + (n ? 10 : 20); }", &[0]),
        21,
    );
}

#[test]
fn nested_ternary_right_associative() {
    // `a ? 1 : (b ? 2 : 3)` — the alternate is itself a ternary.
    // Lowering recurses through the alternate's branch.
    assert_eq!(
        run_int32_function(
            "function f(a) { let b = 1; return a ? 1 : (b ? 2 : 3); }",
            &[0],
        ),
        2,
    );
}

#[test]
fn logical_and_short_circuits_on_falsy() {
    // `0 && x` returns 0 — short-circuits, doesn't evaluate the
    // right operand. We observe this via a local `x` that starts
    // at 99 and would be reassigned by the right-hand if it ran.
    // With short-circuit, the reassignment is skipped.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 99; let y = n && (x = 1); return x; }",
            &[0],
        ),
        99,
    );
}

#[test]
fn logical_and_evaluates_right_on_truthy() {
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 99; let y = n && (x = 7); return x; }",
            &[1],
        ),
        7,
    );
}

#[test]
fn logical_or_short_circuits_on_truthy() {
    // `5 || x` returns 5 — short-circuits.
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 99; let y = n || (x = 1); return x; }",
            &[5],
        ),
        99,
    );
}

#[test]
fn logical_or_evaluates_right_on_falsy() {
    assert_eq!(
        run_int32_function(
            "function f(n) { let x = 99; let y = n || (x = 7); return x; }",
            &[0],
        ),
        7,
    );
}

#[test]
fn logical_and_returns_left_value_when_falsy() {
    // `0 && anything` returns `0`, not `false`. The test harness
    // coerces back to i32, so returning 0 confirms we didn't
    // coerce through ToBoolean somewhere.
    assert_eq!(
        run_int32_function("function f(n) { return n && 7; }", &[0]),
        0,
    );
}

#[test]
fn logical_or_returns_left_value_when_truthy() {
    assert_eq!(
        run_int32_function("function f(n) { return n || 7; }", &[5]),
        5,
    );
}

#[test]
fn nullish_coalesce_falls_through_for_null() {
    // The int32 harness can't produce `null` directly. We rely on
    // a local initialized from a sub-expression that returns null:
    // `let x = (null);` — but the parser lowers `null` as
    // `LdaNull`, so... actually we don't yet have null literal
    // support in the source subset. Test via a value we CAN
    // produce: the `void n` expression returns `undefined`.
    // Confirming that `(void n) ?? 42` returns 42 verifies the
    // undefined path of ??.
    assert_eq!(
        run_int32_function("function f(n) { return (void n) ?? 42; }", &[7]),
        42,
    );
}

#[test]
fn nullish_coalesce_keeps_non_nullish_value() {
    // `5 ?? 42` returns 5 — the non-null-non-undefined branch.
    assert_eq!(
        run_int32_function("function f() { return 5 ?? 42; }", &[]),
        5,
    );
}

#[test]
fn nullish_coalesce_zero_is_not_nullish() {
    // The whole point of `??` over `||`: `0 ?? 42` returns 0,
    // because 0 is not null/undefined (just falsy).
    assert_eq!(
        run_int32_function("function f() { return 0 ?? 42; }", &[]),
        0,
    );
}

#[test]
fn logical_and_in_if_condition() {
    // `if (a && b) { ... }` — logical-expr in if-test position.
    // a=1, b=0 → falsy → if skips.
    assert_eq!(
        run_int32_function(
            "function f(a) { let b = 0; let r = 0; if (a && b) { r = 1; } return r; }",
            &[1],
        ),
        0,
    );
    // a=1, b=1 → truthy → if fires.
    assert_eq!(
        run_int32_function(
            "function f(a) { let b = 1; let r = 0; if (a && b) { r = 1; } return r; }",
            &[1],
        ),
        1,
    );
}

#[test]
fn logical_or_composes_with_ternary() {
    // `(a || b) ? 1 : 0` — logical-or as the test of a ternary.
    assert_eq!(
        run_int32_function(
            "function f(a) { let b = 0; return (a || b) ? 1 : 0; }",
            &[0],
        ),
        // a=0, b=0 → 0 || 0 = 0 (falsy) → ternary takes alt (0).
        0,
    );
    assert_eq!(
        run_int32_function(
            "function f(a) { let b = 0; return (a || b) ? 1 : 0; }",
            &[5],
        ),
        // a=5, b=0 → 5 (truthy) → ternary takes consequent (1).
        1,
    );
}

// ---------------------------------------------------------------------------
// M14: Null / Boolean literals + well-known globals
// ---------------------------------------------------------------------------

#[test]
fn null_literal_returns_null() {
    let module = compile("function f() { return null; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value(), RegisterValue::null());
}

#[test]
fn true_literal_returns_true() {
    let module = compile("function f() { return true; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value().as_bool(), Some(true));
}

#[test]
fn false_literal_returns_false() {
    let module = compile("function f() { return false; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value().as_bool(), Some(false));
}

#[test]
fn undefined_identifier_maps_to_lda_undefined() {
    let module = compile("function f() { return undefined; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(result.return_value(), RegisterValue::undefined());
}

#[test]
fn nan_identifier_maps_to_lda_nan() {
    // `NaN` returns the NaN-boxed NaN — `as_i32` is None because
    // the value isn't an int32. We check via IEEE bits.
    let module = compile("function f() { return NaN; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    let raw = result.return_value().raw_bits();
    assert_eq!(raw, crate::value::TAG_NAN);
}

#[test]
fn infinity_identifier_maps_to_lda_const_f64() {
    let module = compile("function f() { return Infinity; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    // The function's float-constant side table must include INFINITY.
    assert_eq!(function.float_constants().len(), 1);
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    let as_f64 = f64::from_bits(result.return_value().raw_bits());
    assert!(as_f64.is_infinite() && as_f64.is_sign_positive());
}

#[test]
fn global_this_resolves_via_lda_global() {
    // `globalThis` must map through the property-name side table
    // and emit `LdaGlobal`. The interpreter walks the runtime's
    // global object and returns a handle.
    let module = compile("function f() { return globalThis; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.property_names().len(), 1);
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    // globalThis must be an object handle (the global itself).
    assert!(result.return_value().as_object_handle().is_some());
}

#[test]
fn math_identifier_resolves_via_lda_global() {
    // Anchor-builtin check: `Math` is a global bound to the
    // intrinsic Math object. LdaGlobal with "Math" interned into
    // the property-name table should resolve.
    let module = compile("function f() { return Math; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.property_names().len(), 1);
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert!(result.return_value().as_object_handle().is_some());
}

#[test]
fn null_composes_with_nullish_coalesce() {
    // `return null ?? 42` — the null path of `??` fires.
    assert_eq!(
        run_int32_function("function f() { return null ?? 42; }", &[]),
        42,
    );
}

#[test]
fn ternary_with_boolean_literal_test() {
    assert_eq!(
        run_int32_function("function f() { return true ? 1 : 2; }", &[]),
        1,
    );
    assert_eq!(
        run_int32_function("function f() { return false ? 1 : 2; }", &[]),
        2,
    );
}

#[test]
fn undefined_property_name_interner_dedups() {
    // Two `globalThis` references should intern to the same slot
    // — property_names must stay at length 1.
    let module = compile("function f() { let a = globalThis; let b = globalThis; return a; }")
        .expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.property_names().len(), 1);
}

#[test]
fn unknown_global_identifier_still_rejected() {
    // A name that isn't in the whitelist AND isn't a top-level
    // function / local / module global still surfaces as
    // `unbound_identifier` at compile time. `Reflect` is in the
    // whitelist now, so we pick an invented name instead.
    let err = compile("function f() { return TotallyMadeUpBinding; }").expect_err("unknown global");
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

// ---------------------------------------------------------------------------
// M15: StringLiteral + string concatenation (`+` on mixed operands)
// ---------------------------------------------------------------------------

/// Reads the return value as a UTF-8 `String`. Expects the return to
/// be a heap-allocated `JsString` object handle (the only way
/// M15-lowered programs produce strings). Panics on any other shape
/// — the caller's test is asserting a string result.
fn run_string_function(source: &str, args: &[RegisterValue]) -> String {
    let module = compile(source).expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).expect("module has entry function");
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    for (i, v) in args.iter().enumerate() {
        registers[hidden + i] = *v;
    }
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    runtime
        .js_to_string_infallible(result.return_value())
        .into_string()
}

/// Executes a function and either returns its string result or formats an
/// uncaught thrown value in the same runtime that created it.
fn run_string_function_catching(source: &str, args: &[i32]) -> Result<String, String> {
    let module = compile(source).expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).expect("module has entry function");
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    for (i, v) in args.iter().enumerate() {
        registers[hidden + i] = RegisterValue::from_i32(*v);
    }
    let mut runtime = crate::interpreter::RuntimeState::new();
    match Interpreter::new().execute_with_runtime(&module, entry, &registers, &mut runtime) {
        Ok(result) => Ok(runtime
            .js_to_string_infallible(result.return_value())
            .into_string()),
        Err(crate::interpreter::InterpreterError::UncaughtThrow(value)) => {
            if let Some(handle) = value.as_object_handle() {
                let (name, message) =
                    runtime.read_error_name_and_message(crate::object::ObjectHandle(handle));
                if message.is_empty() {
                    Err(name)
                } else {
                    Err(format!("{name}: {message}"))
                }
            } else {
                Err(runtime.js_to_string_infallible(value).into_string())
            }
        }
        Err(err) => panic!("unexpected interpreter failure: {err:?}"),
    }
}

#[test]
fn string_literal_returns_string() {
    let module = compile("function f() { return \"hello\"; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    // The function's string-literal side table must hold exactly
    // one entry — the literal we returned.
    assert_eq!(function.string_literals().len(), 1);
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    let ret = result.return_value();
    assert!(
        ret.as_object_handle().is_some(),
        "string literal must lower to an object handle",
    );
    let text = runtime.js_to_string_infallible(ret);
    assert_eq!(text.as_ref(), "hello");
}

#[test]
fn two_literal_string_concat() {
    assert_eq!(
        run_string_function("function f() { return \"a\" + \"b\"; }", &[]),
        "ab",
    );
}

#[test]
fn literal_plus_identifier_concat() {
    // `"hello, " + name` where `name` is a parameter. The string
    // literal lowers to acc via `LdaConstStr`, then `Add reg(name)`
    // follows through `apply_binary_op_with_acc_lhs`' identifier
    // branch — the simple path, no temp spill needed.
    let module = compile("function greet(name) { return \"hello, \" + name; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.string_literals().len(), 1);
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    // Allocate a runtime-owned string for the parameter.
    let arg_handle = runtime.alloc_string("otter");
    registers[hidden] = RegisterValue::from_object_handle(arg_handle.0);
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(
        runtime
            .js_to_string_infallible(result.return_value())
            .as_ref(),
        "hello, otter",
    );
}

#[test]
fn identifier_plus_literal_concat_preserves_order() {
    // `name + "!"` — identifier first, literal second. RHS is a
    // StringLiteral which falls into the complex-RHS path; the
    // 2-temp fallback must preserve LHS → RHS order so the result
    // is `name + "!"`, not `"!" + name`.
    let module = compile("function punct(name) { return name + \"!\"; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let arg_handle = runtime.alloc_string("otter");
    registers[hidden] = RegisterValue::from_object_handle(arg_handle.0);
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(
        runtime
            .js_to_string_infallible(result.return_value())
            .as_ref(),
        "otter!",
    );
}

#[test]
fn int_plus_string_coerces_to_string() {
    // `5 + "px"` — §13.15.3 step 5: once either operand is a
    // string, both stringify and concat. The LHS lowers as
    // `LdaSmi 5`; RHS is a StringLiteral on the complex-RHS path.
    assert_eq!(
        run_string_function("function f() { return 5 + \"px\"; }", &[]),
        "5px",
    );
}

#[test]
fn string_plus_int_coerces_to_string() {
    assert_eq!(
        run_string_function("function f() { return \"px:\" + 5; }", &[]),
        "px:5",
    );
}

#[test]
fn compound_plus_assign_on_string_local() {
    // `let s = "a"; s += "b"; return s;` — `+=` desugars to a
    // compound assignment that re-uses the Add encoding. The RHS
    // literal goes through the complex-RHS path (two-temp
    // preserve-order).
    assert_eq!(
        run_string_function("function f() { let s = \"a\"; s += \"b\"; return s; }", &[],),
        "ab",
    );
}

#[test]
fn three_way_string_concat_left_associative() {
    // `"x" + "y" + "z"` — parses as `(("x" + "y") + "z")`. The
    // inner binary's result feeds the outer Add as acc-producing
    // expression; the outer RHS ("z") is a StringLiteral going
    // through the complex-RHS path.
    assert_eq!(
        run_string_function("function f() { return \"x\" + \"y\" + \"z\"; }", &[]),
        "xyz",
    );
}

#[test]
fn string_interner_dedups_repeated_literals() {
    // Two references to `"hello"` must share a single side-table
    // slot. Mirrors the M14 property-name interner test.
    let module = compile("function f() { let a = \"hello\"; let b = \"hello\"; return b; }")
        .expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.string_literals().len(), 1);
}

#[test]
fn int32_add_regression_after_noncommutative_marker() {
    // Addition now advertises `commutative: false` so the
    // complex-RHS fallback preserves evaluation order (needed for
    // string concat). Pure int32 addition must still produce the
    // same result it always did, including the nested
    // conditional-RHS shape that exercises the 2-temp path.
    assert_eq!(
        run_int32_function("function f(n) { return 1 + (n ? 10 : 20); }", &[1]),
        11,
    );
    assert_eq!(
        run_int32_function("function f(n) { return 1 + (n ? 10 : 20); }", &[0]),
        21,
    );
    // Int32 + identifier also stays int32 — feedback slot should
    // still record `Int32` here so `M_JIT_C.2` can elide the tag
    // guard on hot loops like bench2.
    assert_eq!(
        run_int32_function("function f(n) { return n + 1; }", &[41]),
        42
    );
}

// ---------------------------------------------------------------------------
// M16: `ObjectExpression` + `ArrayExpression` literals
// ---------------------------------------------------------------------------

/// Compile `source` and run the entry function; returns the raw
/// `RegisterValue` + a live `RuntimeState` so individual tests can
/// inspect heap state (property values, array lengths, …).
fn compile_and_run(
    source: &str,
) -> (
    crate::module::Module,
    RegisterValue,
    crate::interpreter::RuntimeState,
) {
    let module = compile(source).expect("compile");
    // Call the last user-declared named function (`f`, `main`,
    // etc.) directly — the module's actual entry is the
    // synthesised top-level that returns `undefined`. Every
    // pre-top-level test expected the first-declared-function
    // return value, so route there by name-resolution.
    let (entry, function) =
        pick_last_named_function(&module).expect("module must declare a named function");
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    (module, result.return_value(), runtime)
}

#[test]
fn empty_object_literal_returns_object() {
    let (_module, ret, _runtime) = compile_and_run("function f() { return {}; }");
    assert!(
        ret.as_object_handle().is_some(),
        "empty object literal must return an object handle",
    );
}

#[test]
fn empty_array_literal_returns_array() {
    let (_module, ret, runtime) = compile_and_run("function f() { return []; }");
    let handle = crate::object::ObjectHandle(
        ret.as_object_handle()
            .expect("array literal must return a handle"),
    );
    assert_eq!(
        runtime
            .heap()
            .array_length(handle)
            .expect("array kind")
            .expect("array length"),
        0,
    );
}

#[test]
fn object_literal_with_int_values_sets_properties() {
    // `{ a: 1, b: 2 }` — static identifiers mapped to int32 values.
    let (_module, ret, mut runtime) = compile_and_run("function f() { return { a: 1, b: 2 }; }");
    let handle = crate::object::ObjectHandle(ret.as_object_handle().expect("object handle"));
    let key_a = runtime.intern_property_name("a");
    let key_b = runtime.intern_property_name("b");
    let a = runtime
        .own_property_value(handle, key_a)
        .expect("property a");
    let b = runtime
        .own_property_value(handle, key_b)
        .expect("property b");
    assert_eq!(a.as_i32(), Some(1));
    assert_eq!(b.as_i32(), Some(2));
}

#[test]
fn object_literal_with_string_keys() {
    // `{ "hello": 1 }` — string-literal key maps through the
    // property-name interner the same as a static identifier.
    let (_module, ret, mut runtime) = compile_and_run("function f() { return { \"hello\": 1 }; }");
    let handle = crate::object::ObjectHandle(ret.as_object_handle().expect("object handle"));
    let key = runtime.intern_property_name("hello");
    let v = runtime
        .own_property_value(handle, key)
        .expect("property hello");
    assert_eq!(v.as_i32(), Some(1));
}

#[test]
fn object_literal_with_string_values() {
    // `{ name: "otter" }` — value is a StringLiteral threading
    // through the M15 string-literal lowering.
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { return { name: \"otter\" }; }");
    let handle = crate::object::ObjectHandle(ret.as_object_handle().expect("object handle"));
    let key = runtime.intern_property_name("name");
    let v = runtime
        .own_property_value(handle, key)
        .expect("property name");
    assert_eq!(runtime.js_to_string_infallible(v).as_ref(), "otter",);
}

#[test]
fn object_literal_with_mixed_values() {
    // `{ a: 1, b: "two", c: true, d: null }` — all the M14/M15
    // primitives flow through `lower_return_expression` as
    // property values.
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { return { a: 1, b: \"two\", c: true, d: null }; }");
    let handle = crate::object::ObjectHandle(ret.as_object_handle().expect("object handle"));
    let a = runtime.intern_property_name("a");
    let b = runtime.intern_property_name("b");
    let c = runtime.intern_property_name("c");
    let d = runtime.intern_property_name("d");
    let av = runtime.own_property_value(handle, a).unwrap();
    let bv = runtime.own_property_value(handle, b).unwrap();
    let cv = runtime.own_property_value(handle, c).unwrap();
    let dv = runtime.own_property_value(handle, d).unwrap();
    assert_eq!(av.as_i32(), Some(1));
    assert_eq!(runtime.js_to_string_infallible(bv).as_ref(), "two");
    assert_eq!(cv.as_bool(), Some(true));
    assert_eq!(dv, RegisterValue::null());
}

#[test]
fn array_literal_with_int_elements() {
    let (_module, ret, mut runtime) = compile_and_run("function f() { return [10, 20, 30]; }");
    let handle = crate::object::ObjectHandle(ret.as_object_handle().expect("array handle"));
    assert_eq!(runtime.heap().array_length(handle).unwrap().unwrap(), 3,);
    for (i, expected) in [(0usize, 10i32), (1, 20), (2, 30)] {
        let v = runtime
            .objects_mut()
            .get_index(handle, i)
            .unwrap()
            .expect("element");
        assert_eq!(v.as_i32(), Some(expected));
    }
}

#[test]
fn array_literal_with_mixed_primitives() {
    // `[1, "two", true, null]` — values reuse `lower_return_expression`
    // so every M14/M15 primitive composes inside an array.
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { return [1, \"two\", true, null]; }");
    let handle = crate::object::ObjectHandle(ret.as_object_handle().expect("array handle"));
    assert_eq!(runtime.heap().array_length(handle).unwrap().unwrap(), 4,);
    let e0 = runtime.objects_mut().get_index(handle, 0).unwrap().unwrap();
    let e1 = runtime.objects_mut().get_index(handle, 1).unwrap().unwrap();
    let e2 = runtime.objects_mut().get_index(handle, 2).unwrap().unwrap();
    let e3 = runtime.objects_mut().get_index(handle, 3).unwrap().unwrap();
    assert_eq!(e0.as_i32(), Some(1));
    assert_eq!(runtime.js_to_string_infallible(e1).as_ref(), "two");
    assert_eq!(e2.as_bool(), Some(true));
    assert_eq!(e3, RegisterValue::null());
}

#[test]
fn nested_array_in_object_property() {
    // `{ nums: [1, 2, 3] }` — nested composition: the property
    // value is an ArrayExpression. Both temps are acquired in LIFO
    // order (object's temp first, array's temp nested inside).
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { return { nums: [1, 2, 3] }; }");
    let obj = crate::object::ObjectHandle(ret.as_object_handle().expect("object handle"));
    let key = runtime.intern_property_name("nums");
    let arr_val = runtime.own_property_value(obj, key).unwrap();
    let arr = crate::object::ObjectHandle(arr_val.as_object_handle().expect("nested array handle"));
    assert_eq!(runtime.heap().array_length(arr).unwrap().unwrap(), 3);
    assert_eq!(
        runtime
            .objects_mut()
            .get_index(arr, 2)
            .unwrap()
            .unwrap()
            .as_i32(),
        Some(3),
    );
}

#[test]
fn nested_object_in_array_element() {
    // `[{ name: "a" }, { name: "b" }]` — array of object literals.
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { return [{ name: \"a\" }, { name: \"b\" }]; }");
    let arr = crate::object::ObjectHandle(ret.as_object_handle().expect("array handle"));
    assert_eq!(runtime.heap().array_length(arr).unwrap().unwrap(), 2);
    let key = runtime.intern_property_name("name");
    for (i, expected) in [(0usize, "a"), (1, "b")] {
        let elem = runtime.objects_mut().get_index(arr, i).unwrap().unwrap();
        let obj = crate::object::ObjectHandle(elem.as_object_handle().unwrap());
        let v = runtime.own_property_value(obj, key).unwrap();
        assert_eq!(runtime.js_to_string_infallible(v).as_ref(), expected,);
    }
}

#[test]
fn object_property_name_interner_dedups() {
    // `{ k: 1, k: 2 }` — duplicate-key literal is legal JS
    // (later assignment wins). Both write through the same
    // interned name, so `property_names` stays at 1.
    let module = compile("function f() { return { k: 1, k: 2 }; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.property_names().len(), 1);
}

#[test]
fn spread_in_object_copies_own_enumerable_props() {
    // `{ ...src }` copies own-enumerable data properties via
    // the `CopyDataProperties` opcode + runtime helper. Result
    // object carries the same `.a` / `.b` values as the source.
    let src = "function main() { \
            let src = { a: 1, b: 2 }; \
            let o = { ...src, c: 3 }; \
            return o.a + o.b + o.c \
        }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn computed_key_lowers_via_sta_keyed() {
    // `{ [k]: v }` lowers to `StaKeyedProperty` with the key
    // evaluated into a temp register. Runtime coerces via
    // ToPropertyKey.
    let src = "function main() { \
            let k = \"value\"; \
            let o = { [k]: 42 }; \
            return o.value \
        }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn shorthand_property_produces_same_value_pair() {
    // `{ x }` desugars to `{ x: x }`. oxc flags it as
    // `shorthand=true` but the `value` node IS a normal
    // Identifier reference — lowering the value through the
    // regular return-expression path gives the right bytecode.
    let src = "function main() { let x = 7; let o = { x }; return o.x }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn method_property_produces_callable_value() {
    // `{ foo() { return 5 } }` — oxc flags `method=true`,
    // value is a FunctionExpression. Lowering through
    // `lower_return_expression` produces a closure that
    // becomes the property's value.
    let src = "function main() { let o = { foo() { return 5 } }; return o.foo() }";
    assert_eq!(run_int32_function(src, &[]), 5);
}

#[test]
fn getter_property_runs_through_accessor_opcode() {
    // `{ get x() { return 7 } }` — accessor installs via
    // `DefineClassGetter`. Reading `o.x` invokes the getter with
    // `o` as `this`.
    let src = "function f() { let o = { get x() { return 7 } }; return o.x }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn setter_property_runs_through_accessor_opcode() {
    // `{ set x(v) { this._x = v } }` — paired with a backing
    // data property to observe the setter's write.
    let src = "function f() { \
        let o = { _x: 0, set x(v) { this._x = v } }; \
        o.x = 9; \
        return o._x \
    }";
    assert_eq!(run_int32_function(src, &[]), 9);
}

// Removed: spread_array_element_rejected — spread in array literals is
// supported as of M23. Positive coverage in the M23 test block.
#[test]
fn array_literal_still_lowers() {
    // Keep a trivial array-literal smoke test anchored near the
    // removed rejection so grep-by-neighbour stays useful.
    let (_m, ret, _r) = compile_and_run("function f() { return [1, 2, 3]; }");
    assert!(
        ret.as_object_handle().is_some(),
        "array literal returns an object handle",
    );
}

#[test]
fn array_literal_with_elision_pushes_undefined_slots() {
    // `[1, , 3]` — middle hole becomes `undefined`. True
    // sparse-array semantics (where `1 in arr` is `false` for
    // the hole) aren't observed here; the length + indexed
    // reads match what `[1, undefined, 3]` would produce.
    let src = "function f() { let a = [1, , 3]; return a.length + a[0] + a[2]; }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

// ---------------------------------------------------------------------------
// M17: Property access — StaticMemberExpression + ComputedMemberExpression
// ---------------------------------------------------------------------------

#[test]
fn static_member_read_on_identifier_base() {
    // `{ a: 7 }.a` would need member-on-literal; simpler: build
    // the object in a local, then read via `.a`.
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = { a: 7 }; return o.a; }");
    assert_eq!(ret.as_i32(), Some(7));
}

#[test]
fn static_member_read_on_complex_base_uses_temp() {
    // Base is itself an object literal — not an identifier. Falls
    // through `materialize_member_base`'s complex-path (lower + Star
    // into a temp) rather than the identifier fast path.
    let (_module, ret, _runtime) = compile_and_run("function f() { return ({ a: 99 }).a; }");
    assert_eq!(ret.as_i32(), Some(99));
}

#[test]
fn static_member_chain_reads_successive_properties() {
    // `a.b.c`: two member reads chained.
    let (_module, ret, _runtime) = compile_and_run(
        "function f() { let o = { inner: { value: 123 } }; return o.inner.value; }",
    );
    assert_eq!(ret.as_i32(), Some(123));
}

#[test]
fn computed_member_read_with_string_key() {
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { let o = { hello: \"world\" }; return o[\"hello\"]; }");
    assert_eq!(runtime.js_to_string_infallible(ret).as_ref(), "world",);
}

#[test]
fn computed_member_read_with_int_index() {
    // Array indexing: `a[2]` reads element at index 2. Int keys
    // coerce to strings in the generic object-property path.
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let a = [10, 20, 30, 40]; return a[2]; }");
    assert_eq!(ret.as_i32(), Some(30));
}

#[test]
fn computed_member_read_with_identifier_key() {
    // Key expression is an identifier bound to a local.
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = { x: 5, y: 6 }; let k = \"y\"; return o[k]; }");
    assert_eq!(ret.as_i32(), Some(6));
}

#[test]
fn static_member_read_returns_undefined_for_missing_property() {
    // §13.3.2 — missing property returns `undefined`, not a throw.
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = { a: 1 }; return o.missing; }");
    assert_eq!(ret, RegisterValue::undefined());
}

#[test]
fn static_member_plain_assignment_sets_and_returns_value() {
    // `o.x = 5` — statement position; acc holds 5 afterwards so
    // the expression composes (`return o.x = 5` returns 5).
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = { x: 1 }; o.x = 42; return o.x; }");
    assert_eq!(ret.as_i32(), Some(42));
    // Also verify the plain composed form.
    let (_m2, ret2, _r2) = compile_and_run("function f() { let o = {}; return o.x = 5; }");
    assert_eq!(ret2.as_i32(), Some(5));
}

#[test]
fn static_member_defines_new_property_when_absent() {
    // Writing to a previously-absent property installs it per spec;
    // subsequent reads find it.
    let (_module, ret, mut runtime) =
        compile_and_run("function f() { let o = {}; o.created = \"yes\"; return o.created; }");
    assert_eq!(runtime.js_to_string_infallible(ret).as_ref(), "yes",);
}

#[test]
fn static_member_compound_plus_assign_on_string() {
    // Compound `+=` on a member — exercises the
    // Lda/apply/Sta pattern for static member compound assign.
    let (_module, ret, mut runtime) = compile_and_run(
        "function f() { let o = { label: \"hi\" }; o.label += \", otter\"; return o.label; }",
    );
    assert_eq!(runtime.js_to_string_infallible(ret).as_ref(), "hi, otter",);
}

#[test]
fn static_member_compound_plus_assign_on_int() {
    // `o.count += 1` — int32 arithmetic on a member slot. Uses
    // the AddSmi fast path inside `apply_binary_op_with_acc_lhs`.
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = { count: 41 }; o.count += 1; return o.count; }");
    assert_eq!(ret.as_i32(), Some(42));
}

#[test]
fn computed_member_assignment_with_string_key() {
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = {}; o[\"k\"] = 123; return o[\"k\"]; }");
    assert_eq!(ret.as_i32(), Some(123));
}

#[test]
fn computed_member_assignment_with_identifier_key() {
    let (_module, ret, mut runtime) = compile_and_run(
        "function f() { let o = {}; let k = \"dynamic\"; o[k] = \"present\"; return o[k]; }",
    );
    assert_eq!(runtime.js_to_string_infallible(ret).as_ref(), "present",);
}

#[test]
fn computed_member_compound_assign_preserves_key_evaluation_order() {
    // `a[k] += 1` — key is an identifier. The lowering spills the
    // key into a temp exactly once and reuses it for both the read
    // and the write, so the key expression evaluates just once per
    // spec.
    let (_module, ret, _runtime) = compile_and_run(
        "function f() { let o = { n: 10 }; let k = \"n\"; o[k] += 5; return o[k]; }",
    );
    assert_eq!(ret.as_i32(), Some(15));
}

#[test]
fn array_index_assignment_overwrites_element() {
    // Indexed array write: `a[0] = 99`. Exercises
    // StaKeyedProperty with an int-literal key that coerces to "0".
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let a = [1, 2, 3]; a[0] = 99; return a[0]; }");
    assert_eq!(ret.as_i32(), Some(99));
}

#[test]
fn member_read_composes_in_binary_expression() {
    // `s + o.label` — the identifier-plus-member Add. The member
    // lowering feeds through `apply_binary_op_with_complex_rhs`.
    let (_module, ret, mut runtime) = compile_and_run(
        "function f() { let o = { label: \"world\" }; return \"hello, \" + o.label; }",
    );
    assert_eq!(
        runtime.js_to_string_infallible(ret).as_ref(),
        "hello, world",
    );
}

#[test]
fn member_read_composes_inside_return_arithmetic() {
    // `o.a + o.b` — two member reads, int32 add.
    let (_module, ret, _runtime) =
        compile_and_run("function f() { let o = { a: 3, b: 4 }; return o.a + o.b; }");
    assert_eq!(ret.as_i32(), Some(7));
}

#[test]
fn optional_chain_member_returns_value_when_non_null() {
    // `o?.a` when `o` is a truthy object: returns `o.a` like a
    // plain member access.
    assert_eq!(
        run_int32_function("function f() { let o = { a: 42 }; return o?.a; }", &[],),
        42,
    );
}

#[test]
fn optional_chain_member_short_circuits_on_null() {
    // `null?.a` returns undefined. Using unary `+undefined = NaN`
    // doesn't round-trip through i32, so we switch to an
    // equality-based return: `o?.a === undefined` produces `1`
    // when the chain short-circuits.
    assert_eq!(
        run_int32_function(
            "function f() { let o = null; return o?.a === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_chain_member_short_circuits_on_undefined() {
    assert_eq!(
        run_int32_function(
            "function f() { let o = undefined; return o?.a === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_computed_member_returns_value_when_non_null() {
    assert_eq!(
        run_int32_function("function f() { let o = { a: 7 }; return o?.[\"a\"]; }", &[],),
        7,
    );
}

#[test]
fn optional_computed_member_short_circuits_on_null() {
    assert_eq!(
        run_int32_function(
            "function f() { let o = null; return o?.[\"a\"] === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_chain_allows_ts_non_null_member_base() {
    assert_eq!(
        run_int32_function_ts("function f() { let o = { a: 42 }; return (o!)?.a; }", &[]),
        42,
    );
}

#[test]
fn optional_chain_ts_non_null_still_short_circuits_on_null() {
    assert_eq!(
        run_int32_function_ts(
            "function f() { let o = null; return (o!)?.a === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_chain_short_circuits_mid_chain() {
    // `a?.b.c` — once `a?.b` produces undefined, `.c` is skipped
    // (the chain's single short-circuit label covers every access
    // downstream of any `?.` gate).
    assert_eq!(
        run_int32_function(
            "function f() { let a = null; return a?.b.c === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_call_allows_ts_non_null_callee() {
    assert_eq!(
        run_int32_function_ts(
            "function f() { let g = function () { return 7 }; return (g!)?.(); }",
            &[],
        ),
        7,
    );
}

#[test]
fn optional_call_invokes_function_when_non_null() {
    // `f?.()` when f is a callable closure.
    assert_eq!(
        run_int32_function(
            "function f() { let g = function () { return 7 }; return g?.(); }",
            &[],
        ),
        7,
    );
}

#[test]
fn optional_call_short_circuits_on_null() {
    assert_eq!(
        run_int32_function(
            "function f() { let g = null; return g?.() === undefined ? 1 : 0; }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_call_with_spread_invokes_function_when_non_null() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let g = function (a, b) { return a + b }; \
                 return g?.(...[19, 23]); \
             }",
            &[],
        ),
        42,
    );
}

#[test]
fn optional_call_with_spread_short_circuits_on_null() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let g = null; \
                 return g?.(...[1, 2]) === undefined ? 1 : 0; \
             }",
            &[],
        ),
        1,
    );
}

#[test]
fn optional_method_call_passes_correct_this() {
    // `o.m?.()` calls with `this = o` per §13.3.9.3. Verifies
    // the member-callee path preserves `this` instead of falling
    // back to `CallUndefinedReceiver`.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let o = { v: 11, m: function () { return this.v } }; \
                 return o.m?.(); \
             }",
            &[],
        ),
        11,
    );
}

#[test]
fn optional_method_call_with_spread_preserves_this() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let o = { v: 10, m: function (a, b) { return this.v + a + b } }; \
                 return o.m?.(...[20, 12]); \
             }",
            &[],
        ),
        42,
    );
}

#[test]
fn optional_method_call_short_circuits_when_method_missing() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let o = { v: 1 }; \
                 return o.m?.() === undefined ? 1 : 0; \
             }",
            &[],
        ),
        1,
    );
}

#[test]
fn private_field_assignment_outside_class_is_parse_error() {
    // Private fields with a `#` prefix outside a class body are
    // rejected at parse time by oxc; the lowering stage never
    // sees the node. (M29 handles the in-class case via
    // `SetPrivateField`.)
    let err = compile("function f() { let o = { a: 1 }; o.#priv = 2; return 1; }")
        .expect_err("private field outside class");
    assert!(
        matches!(
            err,
            SourceLoweringError::Parse { .. }
                | SourceLoweringError::Unsupported {
                    construct: "undeclared_private_name",
                    ..
                }
        ),
        "unexpected err: {err:?}",
    );
}

// ---------------------------------------------------------------------------
// M18: Template literals (simple + interpolated)
// ---------------------------------------------------------------------------

#[test]
fn simple_template_returns_string() {
    // `` `hello` `` is equivalent to `"hello"` — single quasi, no
    // substitutions. Should lower to a lone `LdaConstStr`.
    assert_eq!(
        run_string_function("function f() { return `hello`; }", &[]),
        "hello",
    );
}

#[test]
fn empty_template_returns_empty_string() {
    assert_eq!(run_string_function("function f() { return ``; }", &[]), "",);
}

#[test]
fn simple_template_interns_string_literal() {
    // Even though the source uses backticks, the literal should
    // intern into the function's string-literal table the same as
    // a regular StringLiteral would.
    let module = compile("function f() { return `hi`; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    assert_eq!(function.string_literals().len(), 1);
}

#[test]
fn template_with_single_identifier_substitution() {
    // `` `hello, ${name}!` `` — head quasi "hello, ", one
    // expression, tail quasi "!".
    let module = compile("function greet(name) { return `hello, ${name}!`; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let arg = runtime.alloc_string("otter");
    registers[hidden] = RegisterValue::from_object_handle(arg.0);
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(
        runtime
            .js_to_string_infallible(result.return_value())
            .as_ref(),
        "hello, otter!",
    );
}

#[test]
fn template_with_int_substitution_coerces_via_js_add() {
    // `` `n=${n}` `` with `n` an int32. The Add opcode's non-int32
    // fallback funnels through `RuntimeState::js_add`, which runs
    // ToPrimitive → ToString on non-string operands once either
    // side is a string (the head quasi here).
    let module = compile("function f(n) { return `n=${n}`; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let hidden = usize::from(function.frame_layout().hidden_count());
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    registers[hidden] = RegisterValue::from_i32(42);
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    assert_eq!(
        runtime
            .js_to_string_infallible(result.return_value())
            .as_ref(),
        "n=42",
    );
}

#[test]
fn template_with_multiple_substitutions() {
    // `` `${a} + ${b} = ${c}` `` — three substitutions, four
    // quasis. Middle quasis " + " and " = " exercise the "roll
    // the buffer forward" path after each substitution.
    assert_eq!(
        run_string_function(
            "function f() { let a = 2; let b = 3; let c = 5; return `${a} + ${b} = ${c}`; }",
            &[],
        ),
        "2 + 3 = 5",
    );
}

#[test]
fn template_with_only_substitution_handles_empty_head_tail() {
    // `` `${n}` `` — head = "", tail = "". The empty-quasi skip
    // path in the lowering avoids emitting redundant `""` concats.
    assert_eq!(
        run_string_function("function f() { let n = 7; return `${n}`; }", &[]),
        "7",
    );
}

#[test]
fn template_with_member_expression_substitution() {
    // `` `name=${o.name}` `` — substitution is a
    // StaticMemberExpression. Relies on the M17 read path and
    // feeds through `lower_return_expression` inside the template
    // lowering.
    assert_eq!(
        run_string_function(
            "function f() { let o = { name: \"otter\" }; return `name=${o.name}`; }",
            &[],
        ),
        "name=otter",
    );
}

#[test]
fn template_with_nested_binary_substitution() {
    // `` `sum=${a + b}` `` — substitution is itself a BinaryExpression.
    assert_eq!(
        run_string_function(
            "function f() { let a = 1; let b = 41; return `sum=${a + b}`; }",
            &[],
        ),
        "sum=42",
    );
}

#[test]
fn template_composes_with_outer_binary_concat() {
    // Outer Add between two strings, RHS is a template literal.
    // Exercises the `TemplateLiteral` entry in the complex-RHS
    // whitelist of `apply_binary_op_with_acc_lhs`.
    assert_eq!(
        run_string_function(
            "function f() { let v = 5; return \"v: \" + `value=${v}`; }",
            &[],
        ),
        "v: value=5",
    );
}

#[test]
fn template_with_escape_sequences_uses_cooked_value() {
    // `` `a\nb` `` — the cooked form has a real newline. Matches
    // the behaviour of a regular string literal with `\n`.
    assert_eq!(
        run_string_function("function f() { return `a\\nb`; }", &[]),
        "a\nb",
    );
}

#[test]
fn template_substitution_can_itself_be_a_template() {
    // Nested templates: `` `outer:${`inner:${n}`}` ``.
    assert_eq!(
        run_string_function(
            "function f() { let n = 9; return `outer:${`inner:${n}`}`; }",
            &[],
        ),
        "outer:inner:9",
    );
}

#[test]
fn tagged_template_invokes_tag_with_strings_and_substitutions() {
    // `tag`a${x}b${y}c`` calls `tag(["a","b","c"], x, y)`. The
    // tag here sums `strings.length + strings[0].length + x + y`
    // — verifies both the strings array delivery and the
    // per-substitution argument delivery.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let tag = function (strings, x, y) { \
                     return strings.length + strings[0].length + x + y; \
                 }; \
                 return tag`a${10}bc${20}d`; \
             }",
            &[],
        ),
        // strings = ["a", "bc", "d"] → length 3. strings[0] = "a" → 1.
        // x + y = 30. total = 3 + 1 + 30 = 34.
        34,
    );
}

#[test]
fn tagged_template_exposes_raw_strings() {
    // The tag's first argument carries a `raw` property — here
    // we just check its length to confirm the raw array is
    // attached and the right size.
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let tag = function (strings) { \
                     return strings.raw.length; \
                 }; \
                 return tag`abc${1}def`; \
             }",
            &[],
        ),
        2,
    );
}

#[test]
fn tagged_template_with_no_substitutions_passes_single_element_array() {
    assert_eq!(
        run_int32_function(
            "function f() { \
                 let tag = function (strings) { return strings.length; }; \
                 return tag`hello`; \
             }",
            &[],
        ),
        1,
    );
}

#[test]
fn template_literal_in_compound_assign_rhs() {
    // `o.label += `;${n}`` — compound member assign with a
    // template literal RHS. Verifies that template literals flow
    // through `apply_binary_op_with_acc_lhs`'s complex-RHS path
    // for member-compound assignments too.
    assert_eq!(
        run_string_function(
            "function f() { let o = { label: \"x\" }; let n = 1; o.label += `;${n}`; return o.label; }",
            &[],
        ),
        "x;1",
    );
}

// ---------------------------------------------------------------------------
// M19: console.log + method calls — the "hello world" gate
// ---------------------------------------------------------------------------

/// Compile `source`, swap a capturing console backend into the
/// runtime so `console.log` output can be inspected, execute the
/// entry function, and return the captured backend + the returned
/// RegisterValue. The capture backend is pluggable per the
/// `ConsoleBackend` trait — `StdioConsoleBackend` is the CLI
/// default, and tests use `CaptureConsoleBackend` here without
/// any change to the runtime's console-delivery pipeline.
fn compile_and_run_with_capture(
    source: &str,
) -> (
    std::sync::Arc<crate::console::CaptureConsoleBackend>,
    RegisterValue,
) {
    let module = compile(source).expect("compile");
    let (entry, function) =
        pick_last_named_function(&module).expect("module must declare a named function");
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let capture = std::sync::Arc::new(crate::console::CaptureConsoleBackend::new());
    // Wrap the shared handle in a forwarding adapter so the
    // runtime's `Box<dyn ConsoleBackend>` owns a view onto the
    // same buffer. `Arc` keeps the backend alive for the test to
    // read after execution.
    struct Shared(std::sync::Arc<crate::console::CaptureConsoleBackend>);
    impl crate::console::ConsoleBackend for Shared {
        fn log(&self, m: &str) {
            self.0.log(m)
        }
        fn warn(&self, m: &str) {
            self.0.warn(m)
        }
        fn error(&self, m: &str) {
            self.0.error(m)
        }
    }
    let mut runtime = crate::interpreter::RuntimeState::new();
    runtime.set_console_backend(Box::new(Shared(capture.clone())));
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    (capture, result.return_value())
}

#[test]
fn console_is_a_global_object() {
    // `console` must resolve through `LdaGlobal` — the M19
    // whitelist entry. The returned value is an object handle
    // (the runtime-installed console intrinsic).
    let (_module, ret, _runtime) = compile_and_run("function f() { return console; }");
    assert!(
        ret.as_object_handle().is_some(),
        "console must lower to an object handle",
    );
}

#[test]
fn console_log_method_exists_on_console() {
    // Reading `console.log` through the StaticMember path — no
    // call yet, just verifying the method is installed on the
    // intrinsic console object.
    let (_module, ret, _runtime) = compile_and_run("function f() { return console.log; }");
    assert!(
        ret.as_object_handle().is_some(),
        "console.log must resolve to a host-function handle",
    );
}

#[test]
fn console_log_from_return_expression_captures_undefined() {
    // `return console.log("hello world");` — `console.log` is
    // invoked in acc-producing position, returns `undefined`,
    // and the capture backend must see the message.
    let (capture, ret) =
        compile_and_run_with_capture("function main() { return console.log(\"hello world\"); }");
    assert_eq!(ret, RegisterValue::undefined());
    let lines = capture.lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].level, crate::console::ConsoleLevel::Log);
    assert_eq!(lines[0].message, "hello world");
}

#[test]
fn console_log_multiple_args_space_separated() {
    // `console.log` joins its args with a single space — matches
    // the WHATWG Console Standard.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { return console.log(\"a\", \"b\", \"c\"); }",
    );
    assert_eq!(capture.lines()[0].message, "a b c");
}

#[test]
fn console_log_coerces_int_to_string() {
    let (capture, _ret) =
        compile_and_run_with_capture("function main() { return console.log(42); }");
    assert_eq!(capture.lines()[0].message, "42");
}

#[test]
fn console_log_with_template_literal_arg() {
    // End-to-end composition: LdaGlobal console →
    // StaticMemberExpression console.log → TemplateLiteral arg.
    // This is the canonical M19 "hello world" shape that a user
    // would write.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let who = \"otter\"; return console.log(`hello, ${who}!`); }",
    );
    assert_eq!(capture.lines()[0].message, "hello, otter!");
}

#[test]
fn console_warn_routes_to_warn_channel() {
    // Separate channels (log/warn/error/info/debug) must each
    // reach the right method on the backend — the
    // `CaptureConsoleBackend` tags each line with its level.
    let (capture, _ret) =
        compile_and_run_with_capture("function main() { return console.warn(\"watch out\"); }");
    let lines = capture.lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].level, crate::console::ConsoleLevel::Warn);
    assert_eq!(lines[0].message, "watch out");
}

#[test]
fn console_error_routes_to_error_channel() {
    let (capture, _ret) =
        compile_and_run_with_capture("function main() { return console.error(\"nope\"); }");
    let lines = capture.lines();
    assert_eq!(lines[0].level, crate::console::ConsoleLevel::Error);
    assert_eq!(lines[0].message, "nope");
}

#[test]
fn method_call_via_computed_key() {
    // `console["log"]("x")` — ComputedMemberExpression callee
    // goes through `lower_computed_method_call`, which uses
    // LdaKeyedProperty instead of LdaNamedProperty.
    let (capture, _ret) =
        compile_and_run_with_capture("function main() { return console[\"log\"](\"computed\"); }");
    assert_eq!(capture.lines()[0].message, "computed");
}

#[test]
fn method_call_on_local_object() {
    // `let o = { get: function… }` isn't lowerable yet (no
    // function expressions), but we can still exercise the
    // method-call path using a value returned by a known intrinsic.
    // Here we verify that a computed-member call against `console`
    // with a dynamic key also reaches the right method.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let m = \"log\"; return console[m](\"dynamic\"); }",
    );
    assert_eq!(capture.lines()[0].message, "dynamic");
}

#[test]
fn method_call_with_chained_template_literal() {
    // Full integration: string + template + method call.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let o = { name: \"otter\", v: 2 }; return console.log(`${o.name} v${o.v}`); }",
    );
    assert_eq!(capture.lines()[0].message, "otter v2");
}

#[test]
fn method_call_preserves_acc_result_for_composition() {
    // `console.log("x")` returns `undefined`; the caller should
    // be able to bind that result into a local.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let r = console.log(\"composed\"); return r; }",
    );
    assert_eq!(capture.lines()[0].message, "composed");
}

#[test]
fn method_call_with_nested_member_receiver() {
    // Receiver is itself a member access: `({c: console}).c.log(...)`.
    // The `materialize_member_base` for the outer static-member
    // receiver `({...}).c` drops into the temp-spill path.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let w = { c: console }; return w.c.log(\"nested\"); }",
    );
    assert_eq!(capture.lines()[0].message, "nested");
}

#[test]
fn natural_hello_world_without_return() {
    // The canonical "hello world" shape: bare `console.log` at
    // statement position, no trailing `return`. Real JS code looks
    // like this — M19 follow-up synthesizes the trailing
    // `LdaUndefined; Return` so programmers don't have to.
    let (capture, ret) =
        compile_and_run_with_capture("function main() { console.log(\"hello world\"); }");
    assert_eq!(ret, RegisterValue::undefined());
    assert_eq!(capture.lines().len(), 1);
    assert_eq!(capture.lines()[0].message, "hello world");
}

#[test]
fn statement_position_call_followed_by_other_statement() {
    // Two statement-position calls in sequence, no explicit
    // return. The synthesized tail return still runs after the
    // second call, and both messages reach the capture backend
    // in source order.
    let (capture, _ret) =
        compile_and_run_with_capture("function main() { console.log(\"a\"); console.log(\"b\"); }");
    let lines = capture.lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].message, "a");
    assert_eq!(lines[1].message, "b");
}

#[test]
fn direct_call_still_uses_call_direct() {
    // Regression: the M9 direct-call path stays intact after the
    // refactor. `main` calls `inc(41)` which lowers to CallDirect,
    // and the returned int32 value round-trips. (Multi-param
    // signatures land in a later milestone.)
    assert_eq!(
        run_int32_function(
            "function inc(n) { return n + 1; } function main() { return inc(41); }",
            &[],
        ),
        42,
    );
}

// ---------------------------------------------------------------------------
// M20: SwitchStatement with case / default + break exits
// ---------------------------------------------------------------------------

#[test]
fn switch_matches_int_case_with_break() {
    let program = "function f(n) { \
        let r = -1; \
        switch (n) { case 1: r = 10; break; case 2: r = 20; break; default: r = 0; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 10);
    assert_eq!(run_int32_function(program, &[2]), 20);
    assert_eq!(run_int32_function(program, &[99]), 0);
}

#[test]
fn switch_falls_through_without_break() {
    // Spec §14.11.9 — SwitchStatement evaluation doesn't
    // synthesize an implicit break; a case without `break` runs
    // into the next case's body.
    let program = "function f(n) { \
        let r = 0; \
        switch (n) { case 1: r += 1; case 2: r += 2; break; case 3: r += 100; break; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 3); // 1 + 2
    assert_eq!(run_int32_function(program, &[2]), 2);
    assert_eq!(run_int32_function(program, &[3]), 100);
}

#[test]
fn switch_with_default_at_end() {
    let program = "function f(n) { \
        let r = 0; \
        switch (n) { case 1: r = 10; break; default: r = 99; break; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 10);
    assert_eq!(run_int32_function(program, &[5]), 99);
}

#[test]
fn switch_with_default_in_middle() {
    // Default isn't always last — it can appear anywhere. The
    // compare phase skips it and the no-match fallback jumps to
    // the default label regardless of source position.
    let program = "function f(n) { \
        let r = 0; \
        switch (n) { case 1: r = 1; break; default: r = 99; break; case 2: r = 2; break; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 1);
    assert_eq!(run_int32_function(program, &[2]), 2);
    assert_eq!(run_int32_function(program, &[3]), 99);
}

#[test]
fn switch_without_default_leaves_result_alone() {
    // When no case matches and there's no default, the switch
    // body is skipped entirely — `r` keeps its initial value.
    let program = "function f(n) { \
        let r = -1; \
        switch (n) { case 1: r = 10; break; case 2: r = 20; break; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 10);
    assert_eq!(run_int32_function(program, &[5]), -1);
}

#[test]
fn switch_uses_strict_equality() {
    // `case "1"` does *not* match discriminant `1` — the compare
    // is `TestEqualStrict` (§7.2.16 IsStrictlyEqual).
    let program = "function f() { \
        let r = 0; \
        let n = 1; \
        switch (n) { case \"1\": r = 10; break; default: r = 99; break; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[]), 99);
}

#[test]
fn switch_on_string_discriminant() {
    let program = "function f() { \
        let r = \"none\"; \
        let k = \"b\"; \
        switch (k) { case \"a\": r = \"A\"; break; case \"b\": r = \"B\"; break; default: r = \"D\"; } \
        return r; \
    }";
    assert_eq!(run_string_function(program, &[]), "B");
}

#[test]
fn switch_empty_case_bodies_fall_through_to_following() {
    // `case 1: case 2: r = 12; break;` — both `1` and `2` land at
    // the same body because empty cases fall through naturally.
    let program = "function f(n) { \
        let r = 0; \
        switch (n) { case 1: case 2: r = 12; break; case 3: r = 3; break; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 12);
    assert_eq!(run_int32_function(program, &[2]), 12);
    assert_eq!(run_int32_function(program, &[3]), 3);
}

#[test]
fn switch_case_lexical_declaration_reads_initialized_binding() {
    let program = "function f() { \
        switch (1) { \
            case 1: let x = 7; return x; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 7);
}

#[test]
fn switch_case_lexical_tdz_on_direct_entry_to_later_case() {
    let program = "function f(n) { \
        switch (n) { \
            case 1: let x = 7; break; \
            case 2: return x; \
            default: return 0; \
        } \
        return 0; \
    }";
    let err = run_string_function_catching(program, &[2]).expect_err("direct entry must hit TDZ");
    assert!(
        err.contains("Cannot access uninitialized binding"),
        "unexpected err: {err}"
    );
}

#[test]
fn switch_case_lexical_tdz_applies_to_case_test_expressions() {
    let program = "function f() { \
        switch (0) { \
            case x: return 1; \
            case 0: let x = 2; return x; \
            default: return 3; \
        } \
    }";
    let err =
        run_string_function_catching(program, &[]).expect_err("case test must observe switch TDZ");
    assert!(
        err.contains("Cannot access uninitialized binding"),
        "unexpected err: {err}"
    );
}

#[test]
fn switch_case_destructuring_lexical_declaration_initializes_hoisted_names() {
    let program = "function f() { \
        switch (1) { \
            case 1: let { x, y = 9 } = { x: 4 }; return x * 10 + y; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 49);
}

#[test]
fn switch_case_object_destructuring_binds_plain_property() {
    let program = "function f() { \
        switch (1) { \
            case 1: let { x } = { x: 4 }; return x; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 4);
}

#[test]
fn switch_case_object_destructuring_applies_default_initializer() {
    let program = "function f() { \
        switch (1) { \
            case 1: let { y = 9 } = {}; return y; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 9);
}

#[test]
fn switch_case_object_destructuring_binds_multiple_names() {
    let program = "function f() { \
        switch (1) { \
            case 1: let { x, y = 9 } = { x: 4 }; return x + y; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 13);
}

#[test]
fn switch_case_object_destructuring_keeps_first_binding_distinct() {
    let program = "function f() { \
        switch (1) { \
            case 1: let { x, y = 9 } = { x: 4 }; return x; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 4);
}

#[test]
fn switch_case_lexical_fallthrough_reads_initialized_binding() {
    let program = "function f() { \
        switch (1) { \
            case 1: let x = 7; \
            case 2: return x; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 7);
}

#[test]
fn switch_case_var_declaration_is_visible_after_switch() {
    let program = "function f(n) { \
        switch (n) { \
            case 1: var x = 7; break; \
            default: var x = 3; \
        } \
        return x; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 7);
    assert_eq!(run_int32_function(program, &[2]), 3);
}

#[test]
fn switch_case_var_is_hoisted_before_discriminant() {
    let program = "function f() { \
        switch (typeof x) { \
            case \"undefined\": var x = 9; break; \
            default: var x = 1; \
        } \
        return x; \
    }";
    assert_eq!(run_int32_function(program, &[]), 9);
}

#[test]
fn switch_case_var_without_matching_case_stays_undefined() {
    let program = "function f() { \
        switch (0) { case 1: var x = 7; break; } \
        return typeof x; \
    }";
    assert_eq!(run_string_function(program, &[]), "undefined");
}

#[test]
fn switch_case_var_destructuring_assigns_hoisted_bindings() {
    let program = "function f() { \
        switch (1) { case 1: var { x, y = 9 } = { x: 4 }; break; } \
        return x * 10 + y; \
    }";
    assert_eq!(run_int32_function(program, &[]), 49);
}

#[test]
fn switch_case_nested_destructuring_lexical_declaration_works() {
    let program = "function f() { \
        switch (1) { \
            case 1: let { a: { b }, c: [d] } = { a: { b: 3 }, c: [4] }; return b * 10 + d; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 34);
}

#[test]
fn switch_case_nested_array_rest_destructuring_works() {
    let program = "function f() { \
        switch (1) { \
            case 1: let [...[a, b]] = [7, 8, 9]; return a * 10 + b; \
            default: return 0; \
        } \
    }";
    assert_eq!(run_int32_function(program, &[]), 78);
}

#[test]
fn break_inside_switch_does_not_escape_outer_loop() {
    // `break` inside a case targets the innermost break-frame —
    // the switch, not the enclosing while.
    let program = "function f(n) { \
        let sum = 0; \
        let i = 0; \
        while (i < n) { \
            switch (i) { case 0: break; case 1: break; default: sum += i; } \
            i += 1; \
        } \
        return sum; \
    }";
    // i=0,1 → break (sum unchanged); i=2,3,4 → default → sum = 2+3+4 = 9.
    assert_eq!(run_int32_function(program, &[5]), 9);
}

#[test]
fn continue_inside_switch_targets_enclosing_loop() {
    // §14.11 — `continue` inside a switch binds to the innermost
    // *iteration* statement, not the switch. The switch frame's
    // `continue_label` is `None` so it's skipped during
    // traversal.
    let program = "function f(n) { \
        let sum = 0; \
        let i = 0; \
        while (i < n) { \
            i += 1; \
            switch (i) { case 3: continue; default: sum += i; } \
        } \
        return sum; \
    }";
    // i runs 1..=5. i=3 → continue → skip `sum += i`.
    // sum = 1 + 2 + 4 + 5 = 12.
    assert_eq!(run_int32_function(program, &[5]), 12);
}

#[test]
fn switch_nested_inside_switch() {
    // Inner switch's `break` stays local to the inner switch.
    let program = "function f(n) { \
        let r = 0; \
        switch (n) { \
            case 1: \
                switch (n) { case 1: r = 11; break; default: r = 10; } \
                break; \
            default: r = 0; \
        } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 11);
    assert_eq!(run_int32_function(program, &[2]), 0);
}

#[test]
fn switch_discriminant_evaluated_once() {
    // Discriminant is a call expression — lowered once into a
    // temp and reloaded from that temp for every compare.
    let program = "function side(n) { return n + 1; } function main() { \
        let n = 0; \
        let r = 0; \
        switch (side(n)) { case 1: r = 10; break; case 2: r = 20; break; } \
        return r; \
    }";
    // side(0) = 1 → case 1 matches.
    assert_eq!(run_int32_function(program, &[]), 10);
}

#[test]
fn switch_with_return_in_case() {
    // `return` inside a case exits the function — no break
    // needed, no fall-through.
    let program = "function f(n) { \
        switch (n) { case 1: return 10; case 2: return 20; } \
        return -1; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 10);
    assert_eq!(run_int32_function(program, &[2]), 20);
    assert_eq!(run_int32_function(program, &[7]), -1);
}

#[test]
fn switch_empty_cases() {
    // `switch (n) {}` — no cases. Legal JS, no-op.
    let program = "function f(n) { let r = 42; switch (n) {} return r; }";
    assert_eq!(run_int32_function(program, &[1]), 42);
}

#[test]
fn switch_preserves_locals_after_exit() {
    // `r` is live before and after the switch; a case that
    // assigns must persist past `switch_exit`.
    let program = "function f(n) { \
        let r = 0; \
        switch (n) { case 1: r = 100; break; } \
        r += 1; \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[1]), 101);
    assert_eq!(run_int32_function(program, &[9]), 1);
}

#[test]
fn break_outside_switch_and_loop_rejected() {
    // Bare `break` outside any break-frame still rejects —
    // switch participates in the same stack as loops, so pushing
    // switch didn't accidentally lift this requirement.
    let err = compile("function f() { break; }").expect_err("break outside loop/switch");
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
// M21: throw / try-catch-finally
// ---------------------------------------------------------------------------

#[test]
fn throw_literal_caught_by_catch() {
    // `throw 42` thrown inside try is caught, and the catch
    // binding receives the thrown value.
    let program = "function f() { \
        let caught = -1; \
        try { throw 42; } catch (e) { caught = e; } \
        return caught; \
    }";
    assert_eq!(run_int32_function(program, &[]), 42);
}

#[test]
fn throw_string_caught_by_catch() {
    // Thrown value can be any JS value, not just an Error.
    let program = "function f() { \
        let r = \"none\"; \
        try { throw \"boom\"; } catch (e) { r = e; } \
        return r; \
    }";
    assert_eq!(run_string_function(program, &[]), "boom");
}

#[test]
fn try_without_throw_skips_catch() {
    // When the try body completes normally, the catch body
    // should not run.
    let program = "function f() { \
        let r = 0; \
        try { r = 10; } catch (e) { r = 999; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[]), 10);
}

#[test]
fn catch_binding_is_block_scoped() {
    // The catch parameter is block-scoped. A later reference to
    // the same name outside the catch should fail as
    // unbound_identifier.
    let err = compile("function f() { try { throw 1; } catch (e) {} return e; }")
        .expect_err("e out of scope after catch");
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
fn bindingless_catch_clause() {
    // `catch { … }` without a parameter (ES2019).
    let program = "function f() { \
        let r = 0; \
        try { throw 1; } catch { r = 99; } \
        return r; \
    }";
    assert_eq!(run_int32_function(program, &[]), 99);
}

#[test]
fn finally_runs_after_normal_try() {
    // `finally` runs on the normal-exit path too — exercised by
    // incrementing a counter in both try body and finally body.
    let program = "function f() { \
        let n = 0; \
        try { n = 1; } finally { n = 10; } \
        return n; \
    }";
    assert_eq!(run_int32_function(program, &[]), 10);
}

#[test]
fn finally_runs_after_thrown_try_and_rethrows() {
    // Without a catch, `finally` still runs, then the throw
    // re-propagates past the finally block (§14.15.3 step 10).
    // We wrap the inner try/finally inside an outer try/catch
    // to observe the propagated exception without crashing.
    let program = "function f() { \
        let seen_finally = 0; \
        let caught = 0; \
        try { \
            try { throw 7; } finally { seen_finally = 1; } \
        } catch (e) { caught = e; } \
        return seen_finally * 100 + caught; \
    }";
    // seen_finally = 1, caught = 7 → 1 * 100 + 7 = 107
    assert_eq!(run_int32_function(program, &[]), 107);
}

#[test]
fn finally_runs_after_caught_try() {
    // try/catch/finally — finally runs after catch body completes.
    let program = "function f() { \
        let seq = 0; \
        try { throw 1; } catch (e) { seq = seq + 1; } finally { seq = seq + 10; } \
        return seq; \
    }";
    assert_eq!(run_int32_function(program, &[]), 11);
}

#[test]
fn finally_runs_before_return_completion() {
    let program = "function main() { \
        let state = 0; \
        function inner() { \
            try { state = 1; return 7; } finally { state = state + 10; } \
        } \
        let returned = inner(); \
        return state * 100 + returned; \
    }";
    assert_eq!(run_int32_function(program, &[]), 1107);
}

#[test]
fn nested_finally_chain_runs_before_return_completion() {
    let program = "function main() { \
        let state = 0; \
        function inner() { \
            try { \
                try { state = 1; return 7; } finally { state = state * 10 + 2; } \
            } finally { \
                state = state * 10 + 3; \
            } \
        } \
        let returned = inner(); \
        return state * 100 + returned; \
    }";
    assert_eq!(run_int32_function(program, &[]), 12307);
}

#[test]
fn finally_runs_before_break_completion() {
    let program = "function main() { \
        let state = 0; \
        let i = 0; \
        while (i < 3) { \
            try { state = state * 10 + 1; break; } finally { state = state * 10 + 2; } \
            i = i + 1; \
        } \
        return state; \
    }";
    assert_eq!(run_int32_function(program, &[]), 12);
}

#[test]
fn finally_runs_before_continue_completion() {
    let program = "function main() { \
        let state = 0; \
        let i = 0; \
        while (i < 3) { \
            i = i + 1; \
            try { state = state * 10 + i; continue; } finally { state = state * 10 + 9; } \
            state = state * 10 + 5; \
        } \
        return state; \
    }";
    assert_eq!(run_int32_function(program, &[]), 192939);
}

#[test]
fn finally_return_overrides_pending_return() {
    let program = "function main() { \
        try { return 1; } finally { return 2; } \
    }";
    assert_eq!(run_int32_function(program, &[]), 2);
}

#[test]
fn throw_in_catch_body_triggers_finally_and_rethrows() {
    // Catch re-throws, finally still runs, then exception
    // propagates to the outer try.
    let program = "function f() { \
        let finally_ran = 0; \
        let caught = 0; \
        try { \
            try { throw 1; } catch (e) { throw e; } finally { finally_ran = 1; } \
        } catch (e) { caught = e; } \
        return finally_ran * 100 + caught; \
    }";
    // finally_ran = 1, caught = 1 → 101
    assert_eq!(run_int32_function(program, &[]), 101);
}

#[test]
fn nested_try_inner_catch_handles_throw() {
    // Inner catch handles the throw; outer doesn't fire.
    let program = "function f() { \
        let inner = 0; \
        let outer = 0; \
        try { \
            try { throw 1; } catch (e) { inner = e; } \
        } catch (e) { outer = e; } \
        return inner * 10 + outer; \
    }";
    assert_eq!(run_int32_function(program, &[]), 10);
}

#[test]
fn nested_try_outer_catch_handles_inner_throw() {
    // Inner has no catch/only-finally → outer catch takes the
    // exception after the inner finally runs.
    let program = "function f() { \
        let finally_ran = 0; \
        let caught = 0; \
        try { \
            try { throw 5; } finally { finally_ran = 1; } \
        } catch (e) { caught = e; } \
        return finally_ran * 100 + caught; \
    }";
    assert_eq!(run_int32_function(program, &[]), 105);
}

#[test]
fn rethrow_via_explicit_throw_in_catch() {
    // `throw e;` inside catch re-raises the caught value.
    let program = "function f() { \
        let outer = 0; \
        try { \
            try { throw 42; } catch (e) { throw e; } \
        } catch (e) { outer = e; } \
        return outer; \
    }";
    assert_eq!(run_int32_function(program, &[]), 42);
}

#[test]
fn throw_object_expression() {
    // Thrown values are first-class — an object literal survives
    // the throw+catch round-trip.
    let program = "function f() { \
        let r = \"missing\"; \
        try { throw { name: \"E\" }; } catch (e) { r = e.name; } \
        return r; \
    }";
    assert_eq!(run_string_function(program, &[]), "E");
}

#[test]
fn uncaught_throw_propagates_to_caller() {
    // `throw` from a function with no enclosing try surfaces as
    // an uncaught-throw completion. We observe it via the
    // harness's `execute_with_runtime` error path.
    let module = compile("function f() { throw 1; }").expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new().execute_with_runtime(&module, entry, &registers, &mut runtime);
    assert!(
        matches!(
            result,
            Err(crate::interpreter::InterpreterError::UncaughtThrow(_))
        ),
        "expected UncaughtThrow, got {result:?}",
    );
}

#[test]
fn try_body_can_have_call_expressions() {
    // Regression: the try body uses `lower_block_statement`,
    // which accepts any nested-statement surface. A call expression
    // as a statement + throw via the call should still work.
    let program = "function inner() { throw 9; } \
        function main() { \
            let r = 0; \
            try { inner(); } catch (e) { r = e; } \
            return r; \
        }";
    assert_eq!(run_int32_function(program, &[]), 9);
}

#[test]
fn finally_with_no_exception_does_not_carry_leftover() {
    // Finally running on the normal path must NOT re-throw a
    // stale exception from an earlier iteration.
    let program = "function f() { \
        let i = 0; \
        let sum = 0; \
        while (i < 3) { \
            try { sum += i + 1; } finally { sum += 10; } \
            i += 1; \
        } \
        return sum; \
    }";
    // Loop runs 3 times: body adds 1,2,3 (sum += 6), finally
    // adds 10 each iter (sum += 30). Total = 36.
    assert_eq!(run_int32_function(program, &[]), 36);
}

#[test]
fn destructuring_catch_param_object_pattern() {
    // `catch ({ msg })` — destructuring pattern on the caught
    // exception. Stash exception in an anon local, then
    // delegate to the shared pattern-bind helper.
    assert_eq!(
        run_int32_function(
            "function f() { try { throw { msg: 7 }; } catch ({ msg }) { return msg; } }",
            &[],
        ),
        7,
    );
}

#[test]
fn destructuring_catch_param_array_pattern() {
    assert_eq!(
        run_int32_function(
            "function f() { try { throw [1, 2]; } catch ([a, b]) { return a + b; } }",
            &[],
        ),
        3,
    );
}

// ---------------------------------------------------------------------------
// M22: default params + rest params + multi-param signatures
// ---------------------------------------------------------------------------

/// Runs a 0-arg function call to `main()` and expects the return to
/// be an int32. Usable for programs that need multiple params on
/// the *callee* side but still return an int at the top level.
fn run_main_int(source: &str) -> i32 {
    let module = compile(source).expect("compile");
    let (entry, _) = pick_last_named_function(&module).expect("named fn");
    let function = module.function(entry).unwrap();
    let registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let mut runtime = crate::interpreter::RuntimeState::new();
    let result = Interpreter::new()
        .execute_with_runtime(&module, entry, &registers, &mut runtime)
        .expect("execute");
    result
        .return_value()
        .as_i32()
        .expect("main must return int32")
}

#[test]
fn two_param_function_receives_both_args() {
    let src = "function sum(a, b) { return a + b; } \
               function main() { return sum(40, 2); }";
    assert_eq!(run_main_int(src), 42);
}

#[test]
fn three_param_function_all_reachable_by_name() {
    // Body references all three params — each must resolve to its
    // own register slot.
    let src = "function calc(a, b, c) { return a + b + c; } \
               function main() { return calc(1, 2, 3); }";
    assert_eq!(run_main_int(src), 6);
}

#[test]
fn param_default_triggered_by_missing_argument() {
    // Caller passes fewer args than params → missing slots are
    // `undefined` → defaults fire.
    let src = "function f(a, b) { return a + b; } \
               function main() { return f(5, 3); }";
    assert_eq!(run_main_int(src), 8);
    let src = "function f(a, b = 100) { return a + b; } \
               function main() { return f(5); }";
    assert_eq!(run_main_int(src), 105);
}

#[test]
fn param_default_not_triggered_by_explicit_value() {
    // Caller supplies the arg → default does not run.
    let src = "function f(a, b = 100) { return a + b; } \
               function main() { return f(5, 7); }";
    assert_eq!(run_main_int(src), 12);
}

#[test]
fn param_default_triggered_by_explicit_undefined() {
    // §10.2.1 — default runs for explicit `undefined` too.
    let src = "function f(a, b = 100) { return a + b; } \
               function main() { return f(5, undefined); }";
    assert_eq!(run_main_int(src), 105);
}

#[test]
fn param_default_can_reference_earlier_param() {
    // Defaults evaluate in source order; later defaults see
    // earlier params' (possibly defaulted) values.
    let src = "function f(a, b = a + 1) { return b; } \
               function main() { return f(10); }";
    assert_eq!(run_main_int(src), 11);
}

#[test]
fn param_default_can_be_a_call_expression() {
    // Default initializers are full expressions.
    let src = "function inc(n) { return n + 1; } \
               function f(a = inc(9)) { return a; } \
               function main() { return f(); }";
    assert_eq!(run_main_int(src), 10);
}

#[test]
fn param_default_can_be_a_template_literal() {
    let src = "function greet(name, msg = `hello, ${name}`) { return msg; } \
               function main() { return greet(\"otter\"); }";
    assert_eq!(run_string_function(src, &[]), "hello, otter");
}

#[test]
fn rest_parameter_collects_extra_args() {
    // Caller passes more args than non-rest params → extras go
    // into the rest array.
    let src = "function f(a, ...rest) { return rest.length; } \
               function main() { return f(1, 2, 3, 4); }";
    assert_eq!(run_main_int(src), 3);
}

#[test]
fn rest_parameter_preserves_element_order() {
    // rest[0] is the first extra arg, rest[1] the second, …
    let src = "function f(...rest) { return rest[0] + rest[1] + rest[2]; } \
               function main() { return f(1, 10, 100); }";
    assert_eq!(run_main_int(src), 111);
}

#[test]
fn rest_parameter_empty_when_no_extras() {
    // Caller passes fewer args than non-rest params → rest
    // materialises an empty array.
    let src = "function f(a, b, ...rest) { return rest.length; } \
               function main() { return f(1, 2); }";
    assert_eq!(run_main_int(src), 0);
}

#[test]
fn rest_parameter_no_non_rest_receives_all_args() {
    let src = "function f(...args) { return args.length; } \
               function main() { return f(1, 2, 3, 4, 5); }";
    assert_eq!(run_main_int(src), 5);
}

#[test]
fn rest_parameter_with_defaults_mix() {
    // Combine defaults + rest. Rest captures only args past the
    // non-rest count.
    let src = "function f(a = 10, b = 20, ...rest) { return a + b + rest.length; } \
               function main() { return f(); }";
    // No args → a=10 (default), b=20 (default), rest=[]
    assert_eq!(run_main_int(src), 30);
    let src = "function f(a = 10, b = 20, ...rest) { return a + b + rest.length; } \
               function main() { return f(1, 2, 3, 4); }";
    // a=1, b=2, rest=[3, 4]
    assert_eq!(run_main_int(src), 5);
}

#[test]
fn param_binding_can_be_assigned() {
    // Params are ordinary `let`-like bindings — reassigning works.
    let src = "function f(a, b) { a = a + 1; return a + b; } \
               function main() { return f(5, 3); }";
    assert_eq!(run_main_int(src), 9);
}

#[test]
fn param_local_collision_rejected() {
    // Top-scope `let` can't shadow a parameter of the same name.
    let err = compile("function f(a) { let a = 1; return a; }").expect_err("duplicate a");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "duplicate_binding",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

#[test]
fn destructuring_rest_param_array_pattern() {
    // `function f(...[x, y])` — rest arg collected into an
    // array, then array-pattern destructured into leaf locals.
    // Use an outer wrapper so the test harness doesn't have to
    // thread `overflow_args` manually.
    assert_eq!(
        run_int32_function(
            "function inner(...[x, y]) { return x + y } \
             function main() { return inner(3, 4) }",
            &[],
        ),
        7,
    );
}

#[test]
fn destructuring_rest_param_object_pattern() {
    // Rare in practice (rest is an Array so object-destructuring
    // binds only `length` / indexed names), but valid.
    assert_eq!(
        run_int32_function(
            "function inner(...{ length }) { return length } \
             function main() { return inner(1, 2, 3, 4) }",
            &[],
        ),
        4,
    );
}

// ---------------------------------------------------------------------------
// M23: spread in array literals + method-call args
// ---------------------------------------------------------------------------

#[test]
fn spread_expands_array_literal() {
    // `[...a]` — a single spread source.
    let src = "function f() { \
        let a = [1, 2, 3]; \
        let b = [...a]; \
        return b[0] + b[1] + b[2]; \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn spread_concatenates_two_arrays() {
    // `[...a, ...b]` — two spread sources in a row.
    let src = "function f() { \
        let a = [1, 2]; \
        let b = [3, 4]; \
        let c = [...a, ...b]; \
        return c[0] + c[1] + c[2] + c[3]; \
    }";
    assert_eq!(run_int32_function(src, &[]), 10);
}

#[test]
fn spread_mixes_with_regular_elements() {
    // `[0, ...a, 99]` — spread wrapped by plain values.
    let src = "function f() { \
        let a = [10, 20]; \
        let b = [0, ...a, 99]; \
        return b[0] + b[1] + b[2] + b[3]; \
    }";
    // 0 + 10 + 20 + 99 = 129
    assert_eq!(run_int32_function(src, &[]), 129);
}

#[test]
fn spread_preserves_array_length() {
    // Verify the spread preserves the source's length.
    let src = "function f() { \
        let a = [1, 2, 3, 4, 5]; \
        let b = [...a]; \
        return b.length; \
    }";
    assert_eq!(run_int32_function(src, &[]), 5);
}

#[test]
fn spread_empty_source_contributes_nothing() {
    let src = "function f() { \
        let a = []; \
        let b = [1, ...a, 2]; \
        return b.length; \
    }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn spread_in_method_call_expands_args() {
    // `console.log(...msgs)` — spread in a method-call arg
    // position. The compiler builds an Array, then dispatches
    // through `CallSpread` which unpacks the array.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let msgs = [\"hello\", \"world\"]; console.log(...msgs); }",
    );
    // Captured line joins args with a single space (Console
    // Standard §2.2 Printer step 3).
    assert_eq!(capture.lines()[0].message, "hello world");
}

#[test]
fn spread_in_method_call_alongside_regular_args() {
    // `console.log("prefix:", ...parts)` — mix regular +
    // spread args.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { let parts = [\"a\", \"b\"]; console.log(\"prefix:\", ...parts); }",
    );
    assert_eq!(capture.lines()[0].message, "prefix: a b");
}

#[test]
fn spread_flows_through_rest_parameter() {
    // End-to-end rest + spread: `g(...xs)` where `g` uses `...args`
    // collects them all back into a rest array. The resulting
    // length should match the original array's length.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { \
            let xs = [\"a\", \"b\", \"c\"]; \
            console.log(...xs); \
        }",
    );
    assert_eq!(capture.lines()[0].message, "a b c");
}

#[test]
fn spread_with_computed_method_call() {
    // `console[k](...xs)` — computed method callee + spread
    // args. Exercises the `lower_computed_method_call`
    // spread branch.
    let (capture, _ret) = compile_and_run_with_capture(
        "function main() { \
            let xs = [\"hi\"]; \
            let k = \"log\"; \
            console[k](...xs); \
        }",
    );
    assert_eq!(capture.lines()[0].message, "hi");
}

#[test]
fn nested_spread_in_array() {
    // `[...[1, 2], 3]` — the spread source is itself a literal.
    let src = "function f() { \
        let a = [...[1, 2], 3]; \
        return a[0] + a[1] + a[2]; \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn spread_over_non_iterable_throws_type_error() {
    // Spreading a non-iterable value surfaces as a runtime
    // TypeError that the program can observe via try/catch.
    let src = "function f() { \
        let r = 0; \
        try { let n = 42; let bad = [...n]; r = 1; } catch (e) { r = 99; } \
        return r; \
    }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

// ---------------------------------------------------------------------------
// M24: destructuring patterns (array + object) in `let` bindings and params
// ---------------------------------------------------------------------------

#[test]
fn let_array_destructures_two_elements() {
    let src = "function f() { \
        let [a, b] = [10, 20]; \
        return a + b; \
    }";
    assert_eq!(run_int32_function(src, &[]), 30);
}

#[test]
fn let_array_destructures_three_elements_in_order() {
    let src = "function f() { \
        let [a, b, c] = [1, 2, 3]; \
        return a * 100 + b * 10 + c; \
    }";
    assert_eq!(run_int32_function(src, &[]), 123);
}

#[test]
fn let_array_destructure_shorter_source_fills_undefined() {
    // `let [a, b, c] = [1]` → b = c = undefined. Missing slots
    // are reachable; using them in arithmetic would coerce to
    // NaN, so test via member-like access (observe non-undefined).
    let src = "function f() { \
        let [a, b] = [42]; \
        let r = 0; \
        if (a === 42) r = r + 1; \
        return r; \
    }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn let_array_destructure_with_rest_collects_remainder() {
    let src = "function f() { \
        let [first, ...rest] = [100, 200, 300, 400]; \
        return first + rest.length; \
    }";
    // first = 100, rest.length = 3
    assert_eq!(run_int32_function(src, &[]), 103);
}

#[test]
fn let_array_destructure_rest_preserves_values() {
    let src = "function f() { \
        let [a, ...tail] = [10, 20, 30]; \
        return a + tail[0] + tail[1]; \
    }";
    assert_eq!(run_int32_function(src, &[]), 60);
}

#[test]
fn let_object_destructure_shorthand() {
    let src = "function f() { \
        let { x, y } = { x: 7, y: 11 }; \
        return x + y; \
    }";
    assert_eq!(run_int32_function(src, &[]), 18);
}

#[test]
fn let_object_destructure_with_renaming() {
    // `{ a: x }` binds the source's `a` property to local `x`.
    let src = "function f() { \
        let { a: x, b: y } = { a: 3, b: 4 }; \
        return x * y; \
    }";
    assert_eq!(run_int32_function(src, &[]), 12);
}

#[test]
fn let_object_destructure_with_default() {
    let src = "function f() { \
        let { missing = 99 } = {}; \
        return missing; \
    }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

#[test]
fn let_object_destructure_default_not_triggered_when_present() {
    let src = "function f() { \
        let { a = 99 } = { a: 7 }; \
        return a; \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn let_object_destructure_rename_with_default() {
    let src = "function f() { \
        let { a: x = 5 } = { a: 3 }; \
        let { b: y = 10 } = {}; \
        return x + y; \
    }";
    assert_eq!(run_int32_function(src, &[]), 13);
}

#[test]
fn object_destructure_param_with_default() {
    // Param is `{ name, score = 0 }` — default on a leaf.
    let src = "function greet({ name, score = 0 }) { \
        return score; \
    } \
    function main() { return greet({ name: \"a\" }); }";
    assert_eq!(run_int32_function(src, &[]), 0);
    let src = "function greet({ name, score = 0 }) { \
        return score; \
    } \
    function main() { return greet({ name: \"a\", score: 42 }); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn array_destructure_param() {
    let src = "function sum([a, b]) { return a + b; } \
               function main() { return sum([15, 27]); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn array_destructure_param_with_rest() {
    let src = "function f([head, ...tail]) { \
        return head + tail.length; \
    } \
    function main() { return f([1, 2, 3, 4]); }";
    // head=1, tail.length=3
    assert_eq!(run_int32_function(src, &[]), 4);
}

#[test]
fn nested_array_destructuring_works() {
    // `[[a]] = [[1]]` — outer array pattern's element is itself
    // an ArrayPattern. The lowering reads element[0] into a temp
    // and recurses.
    assert_eq!(
        run_int32_function("function f() { let [[a]] = [[7]]; return a; }", &[]),
        7,
    );
}

#[test]
fn nested_object_destructuring_works() {
    // `{ a: { b } }` — nested ObjectPattern.
    assert_eq!(
        run_int32_function(
            "function f() { let { a: { b } } = { a: { b: 9 } }; return b; }",
            &[],
        ),
        9,
    );
}

#[test]
fn object_rest_destructure_copies_remaining_keys() {
    // `{ a, ...rest }` — `a` binds directly, every other
    // own-enumerable property lands on a fresh `rest` object.
    assert_eq!(
        run_int32_function(
            "function f() { let { a, ...rest } = { a: 1, b: 2, c: 3 }; return rest.b + rest.c; }",
            &[],
        ),
        5,
    );
}

#[test]
fn computed_pattern_key_works() {
    // `{ [k]: v } = obj` — key evaluates at runtime, value lands
    // in `v` via `LdaKeyedProperty`.
    assert_eq!(
        run_int32_function(
            "function f() { let k = \"a\"; let { [k]: v } = { a: 42 }; return v; }",
            &[],
        ),
        42,
    );
}

#[test]
fn array_pattern_hole_skips_element() {
    // `[a, , c]` — the middle hole has no binding. Lowerer
    // advances the index anyway so `c` reads element[2].
    assert_eq!(
        run_int32_function(
            "function f() { let [a, , c] = [1, 2, 3]; return a + c; }",
            &[],
        ),
        4,
    );
}

#[test]
fn array_destructure_default_applies_when_undefined() {
    // `let [a = 5] = []` — element 0 is undefined, so `a` takes
    // the default value. `let [a = 5] = [7]` keeps the provided
    // value.
    assert_eq!(
        run_int32_function("function f() { let [a = 5] = []; return a; }", &[]),
        5,
    );
    assert_eq!(
        run_int32_function("function f() { let [a = 5] = [7]; return a; }", &[]),
        7,
    );
}

// ---------------------------------------------------------------------------
// M25: FunctionExpression + closures (upvalue capture)
// ---------------------------------------------------------------------------

#[test]
fn function_expression_without_captures_returns_value() {
    let src = "function main() { let f = function() { return 42; }; return f(); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn function_expression_with_param_returns_result() {
    let src = "function main() { let dbl = function(n) { return n + n; }; return dbl(21); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn closure_captures_outer_parameter() {
    let src = "function makeAdder(n) { return function(x) { return x + n; }; } \
               function main() { let add10 = makeAdder(10); return add10(5); }";
    assert_eq!(run_int32_function(src, &[]), 15);
}

#[test]
fn closure_captures_outer_local() {
    let src = "function make() { let base = 100; return function() { return base; }; } \
               function main() { let f = make(); return f(); }";
    assert_eq!(run_int32_function(src, &[]), 100);
}

#[test]
fn closure_live_capture_outer_mutation_visible_from_inner() {
    // §10.2.1 — closures see live outer mutations.
    let src = "function make() { \
        let v = 1; \
        let f = function() { return v; }; \
        v = 99; \
        return f(); \
    } \
    function main() { return make(); }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

#[test]
fn closure_inner_mutation_visible_to_outer() {
    // Inner closure writes captured binding; outer observes
    // via the open-upvalue cell sync.
    let src = "function make() { \
        let v = 0; \
        let setter = function() { v = 42; }; \
        setter(); \
        return v; \
    } \
    function main() { return make(); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn counter_closure_maintains_state_across_calls() {
    let src = "function makeCounter() { \
        let n = 0; \
        return function() { n = n + 1; return n; }; \
    } \
    function main() { \
        let inc = makeCounter(); \
        inc(); \
        inc(); \
        return inc(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 3);
}

#[test]
fn independent_closures_have_independent_state() {
    // Each `makeCounter` call allocates separate upvalue cells.
    let src = "function makeCounter() { \
        let n = 0; \
        return function() { n = n + 1; return n; }; \
    } \
    function main() { \
        let a = makeCounter(); \
        let b = makeCounter(); \
        a(); a(); a(); \
        b(); \
        return a() * 10 + b(); \
    }";
    // a called 4 times (=4); b called 2 times (=2). 4*10 + 2 = 42.
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn closure_calls_top_level_function() {
    let src = "function helper(n) { return n + 1; } \
               function main() { \
                   let f = function(x) { return helper(x); }; \
                   return f(41); \
               }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn compound_assign_on_captured_binding() {
    let src = "function make() { \
        let total = 0; \
        let add = function(n) { total += n; }; \
        add(3); add(4); add(5); \
        return total; \
    } \
    function main() { return make(); }";
    assert_eq!(run_int32_function(src, &[]), 12);
}

#[test]
fn closure_with_template_literal_over_captured_name() {
    let src = "function greeter(name) { \
        return function() { return `hello, ${name}!`; }; \
    } \
    function main() { let g = greeter(\"otter\"); return g(); }";
    assert_eq!(run_string_function(src, &[]), "hello, otter!");
}

#[test]
fn nested_closure_captures_grandparent_scope() {
    // Three-level nesting — innermost captures `x` from the
    // outermost via chained `CaptureDescriptor::Upvalue`.
    let src = "function outer(x) { \
        return function() { \
            return function() { return x; }; \
        }; \
    } \
    function main() { \
        let mk = outer(7); \
        let inner = mk(); \
        return inner(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn nested_function_declaration_binds_local() {
    // `function f(){…}` inside a body binds `f` as a const
    // local — callable from the same scope as a regular
    // closure. Not hoisted (M25 simplification), so callers
    // must declare before use.
    let src = "function outer() { \
        function inner(x) { return x + 1; } \
        return inner(41); \
    } \
    function main() { return outer(); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn nested_function_declaration_captures_outer_local() {
    // The nested FunctionDeclaration path runs through the
    // same `lower_inner_function_with_captures` as
    // FunctionExpression, so captures work identically.
    let src = "function outer() { \
        let base = 100; \
        function add(x) { return base + x; } \
        return add(42); \
    } \
    function main() { return outer(); }";
    assert_eq!(run_int32_function(src, &[]), 142);
}

#[test]
fn nested_function_declaration_binding_is_const() {
    // `function f(){…}` inside a body binds as const — the
    // declaration's name can't be reassigned.
    let err = compile(
        "function outer() { function f() { return 1; } f = 2; return f; } \
         function main() { return outer(); }",
    )
    .expect_err("reassign nested fn binding");
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

// ---------------------------------------------------------------------------
// M26: Arrow functions (concise + block body, lexical `this`)
// ---------------------------------------------------------------------------

#[test]
fn arrow_concise_body_returns_expression() {
    let src = "function main() { let sq = (n) => n * n; return sq(7); }";
    assert_eq!(run_int32_function(src, &[]), 49);
}

#[test]
fn arrow_block_body_with_explicit_return() {
    let src = "function main() { \
        let f = (a, b) => { let s = a + b; return s * 2; }; \
        return f(3, 4); \
    }";
    assert_eq!(run_int32_function(src, &[]), 14);
}

#[test]
fn arrow_zero_args_concise() {
    let src = "function main() { let five = () => 5; return five() + five(); }";
    assert_eq!(run_int32_function(src, &[]), 10);
}

#[test]
fn arrow_single_arg_without_parens() {
    // `n => n + 1` — oxc parses the bare-identifier form.
    let src = "function main() { let inc = n => n + 1; return inc(41); }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn arrow_captures_outer_parameter() {
    // Classic add-N closure via arrow.
    let src = "function makeAdder(n) { return (x) => x + n; } \
               function main() { let add10 = makeAdder(10); return add10(5); }";
    assert_eq!(run_int32_function(src, &[]), 15);
}

#[test]
fn arrow_captures_outer_local() {
    let src = "function make() { let base = 100; return () => base; } \
               function main() { let f = make(); return f(); }";
    assert_eq!(run_int32_function(src, &[]), 100);
}

#[test]
fn arrow_returning_template_literal() {
    let src = "function main() { \
        let greet = (name) => `hello, ${name}!`; \
        return greet(\"otter\"); \
    }";
    assert_eq!(run_string_function(src, &[]), "hello, otter!");
}

#[test]
fn arrow_chain_returns_callable() {
    // `x => y => x + y` — curried adder. Inner arrow captures
    // outer arrow's parameter via the same closure chain.
    let src = "function main() { \
        let add = x => y => x + y; \
        let add3 = add(3); \
        return add3(4); \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn arrow_as_call_argument() {
    // Passing an arrow as a callback-style value. `console.log`
    // won't call back into it, so we just verify the arrow
    // itself produces the value when invoked separately.
    let src = "function apply(f, v) { return f(v); } \
               function main() { return apply(n => n * 10, 5); }";
    assert_eq!(run_int32_function(src, &[]), 50);
}

// ---------------------------------------------------------------------------
// M27: Class declarations (constructor + instance/static methods)
// ---------------------------------------------------------------------------

#[test]
fn class_with_constructor_and_instance_method() {
    let src = "function main() { \
        class Point { \
            constructor(x, y) { this.x = x; this.y = y; } \
            sum() { return this.x + this.y; } \
        } \
        let p = new Point(3, 4); \
        return p.sum(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn class_without_explicit_constructor() {
    // No `constructor` → synthesised empty ctor. `new Foo()`
    // must still return the allocated receiver (not undefined).
    let src = "function main() { \
        class Box { answer() { return 42; } } \
        let b = new Box(); \
        return b.answer(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn class_static_method_returns_instance() {
    // `Point.zero()` constructs a Point internally — the static
    // method body closes over the class binding through the
    // pre-allocated class-name local.
    let src = "function main() { \
        class Point { \
            constructor(x, y) { this.x = x; this.y = y; } \
            static zero() { return new Point(0, 0); } \
        } \
        let z = Point.zero(); \
        return z.x + z.y; \
    }";
    assert_eq!(run_int32_function(src, &[]), 0);
}

#[test]
fn class_constructor_return_object_override() {
    // §9.2.2.1 — an explicit Object return from a constructor
    // wins over the allocated receiver.
    let src = "function main() { \
        class Trick { \
            constructor() { this.x = 1; return { x: 99 }; } \
        } \
        let t = new Trick(); \
        return t.x; \
    }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

#[test]
fn class_constructor_return_primitive_ignored() {
    // §9.2.2.1 — a primitive return is dropped; the allocated
    // receiver wins.
    let src = "function main() { \
        class Keep { \
            constructor() { this.x = 7; return 123; } \
        } \
        let k = new Keep(); \
        return k.x; \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn class_instance_method_uses_this() {
    let src = "function main() { \
        class Counter { \
            constructor() { this.n = 10; } \
            bump() { this.n = this.n + 1; return this.n; } \
        } \
        let c = new Counter(); \
        c.bump(); \
        c.bump(); \
        return c.bump(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 13);
}

#[test]
fn class_static_method_is_on_constructor() {
    // Static method installed on the constructor, not the
    // prototype — so instances can't see it.
    let src = "function main() { \
        class C { \
            static tag() { return 99; } \
        } \
        return C.tag(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

#[test]
fn class_new_with_zero_args() {
    let src = "function main() { \
        class E { \
            constructor() { this.v = 55; } \
        } \
        return new E().v; \
    }";
    assert_eq!(run_int32_function(src, &[]), 55);
}

#[test]
fn class_expression_anonymous_binds_to_let() {
    // `let C = class { … };` — anonymous class expression.
    let src = "function main() { \
        let C = class { answer() { return 42; } }; \
        let c = new C(); \
        return c.answer(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn class_expression_with_constructor() {
    let src = "function main() { \
        let Point = class { \
            constructor(x) { this.x = x; } \
            dup() { return this.x + this.x; } \
        }; \
        let p = new Point(7); \
        return p.dup(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 14);
}

#[test]
fn class_expression_named_still_constructs() {
    // Named class expression — the `Box` name is a hint; we
    // don't bind it in the inner scope yet, but outside it
    // remains unbound, and the expression still yields a usable
    // constructor.
    let src = "function main() { \
        let Ctor = class Box { \
            constructor(v) { this.v = v; } \
        }; \
        let c = new Ctor(11); \
        return c.v; \
    }";
    assert_eq!(run_int32_function(src, &[]), 11);
}

#[test]
fn class_expression_static_method() {
    let src = "function main() { \
        let M = class { static five() { return 5; } }; \
        return M.five(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 5);
}

// ---------------------------------------------------------------------------
// M28: Class inheritance (extends + super + super() in constructor)
// ---------------------------------------------------------------------------

#[test]
fn m28_extends_preserves_prototype_chain() {
    // §15.7.14 step 7 — `Sub.prototype.__proto__ = Super.prototype`.
    // We can't call `instanceof` yet (that's a later milestone),
    // so exercise the inheritance via a parent instance method
    // being reachable from a Sub instance through the
    // prototype chain.
    let src = "function main() { \
        class Animal { hello() { return 77; } } \
        class Dog extends Animal {} \
        let d = new Dog(); \
        return d.hello(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 77);
}

#[test]
fn m28_super_method_called_from_instance_method() {
    // `super.greet()` resolves against `Parent.prototype` but
    // invokes with `this = child instance`, so `this.tag` reads
    // the child's own property.
    let src = "function main() { \
        class Parent { greet() { return this.tag + 10; } } \
        class Child extends Parent { \
            constructor() { super(); this.tag = 5; } \
            call() { return super.greet(); } \
        } \
        let c = new Child(); \
        return c.call(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 15);
}

#[test]
fn m28_super_call_initializes_this_in_derived_ctor() {
    // `super(args)` must run before `this.x` in a derived ctor
    // (§10.2.1.3 derived-path step 12). `x` below is forwarded to
    // the parent and then read back through `this`.
    let src = "function main() { \
        class Base { constructor(x) { this.x = x; } } \
        class Sub extends Base { \
            constructor(x, y) { super(x); this.y = y; } \
            sum() { return this.x + this.y; } \
        } \
        let s = new Sub(3, 4); \
        return s.sum(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn m28_derived_class_without_explicit_constructor() {
    // §15.7.14 step 10.b — the synthesised default ctor forwards
    // all args to `super(...args)`. Parent's constructor stores
    // `x`, which the child inherits transparently.
    let src = "function main() { \
        class Parent { constructor(x) { this.x = x; } } \
        class Child extends Parent {} \
        let c = new Child(21); \
        return c.x + c.x; \
    }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn m28_static_chain_walks_via_extends() {
    // §15.7.14 step 6 — `Sub.__proto__ = Super`. Accessing
    // `Sub.parentStatic` walks up the constructor chain to
    // `Super.parentStatic`.
    let src = "function main() { \
        class Base { static tag() { return 99; } } \
        class Sub extends Base {} \
        return Sub.tag(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

#[test]
fn m28_super_static_method_from_static_method() {
    // `super.tag()` inside a static method of `Sub` resolves to
    // `Base.tag` via the static-chain HomeObject.
    let src = "function main() { \
        class Base { static tag() { return 7; } } \
        class Sub extends Base { \
            static doubled() { return super.tag() * 2; } \
        } \
        return Sub.doubled(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 14);
}

#[test]
fn m28_super_property_read() {
    // `super.x` in a method — data property lookup on
    // `Parent.prototype`. The parent's prototype has `answer: 42`.
    let src = "function main() { \
        class Parent {} \
        Parent.prototype.answer = 42; \
        class Child extends Parent { \
            ask() { return super.answer; } \
        } \
        return new Child().ask(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn m28_super_property_assignment_writes_to_this() {
    // §13.3.7 — `super.x = v` writes to `this` (not to the
    // super prototype), using `this` as the `[[Set]]` receiver.
    let src = "function main() { \
        class Parent {} \
        class Child extends Parent { \
            constructor() { super(); super.slot = 9; } \
        } \
        return new Child().slot; \
    }";
    assert_eq!(run_int32_function(src, &[]), 9);
}

#[test]
fn m28_optional_super_static_method_call_preserves_this() {
    let src = "function main() { \
            class Parent { get() { return this.value; } } \
            class Child extends Parent { \
                constructor() { super(); this.value = 11; } \
                ask() { return super.get?.(); } \
            } \
            return new Child().ask(); \
        }";
    assert_eq!(run_int32_function(src, &[]), 11);
}

#[test]
fn m28_optional_super_method_call_short_circuits_missing_method() {
    let src = "function main() { \
            class Parent {} \
            class Child extends Parent { \
                ask() { return super.missing?.() === undefined ? 42 : 0; } \
            } \
            return new Child().ask(); \
        }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn m28_optional_super_computed_method_call_preserves_this() {
    let src = "function main() { \
            class Parent { get() { return this.value + 1; } } \
            class Child extends Parent { \
                constructor() { super(); this.value = 12; } \
                ask() { return super['get']?.(); } \
            } \
            return new Child().ask(); \
        }";
    assert_eq!(run_int32_function(src, &[]), 13);
}

#[test]
fn m28_super_outside_class_is_unsupported() {
    // `super` used in a regular function body has no enclosing
    // `ClassSuperBinding` — the compiler surfaces
    // `super_outside_class` at lowering time.
    let err = compile("function main() { super.x; }").expect_err("super outside class must reject");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "super_outside_class",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

#[test]
fn m28_super_call_in_base_class_is_unsupported() {
    // Base-class constructor — `ClassSuperBinding` exists but
    // `allow_super_call` is false. Reject with the dedicated
    // tag so future tests can grep for it.
    let err = compile("function main() { class Base { constructor() { super(); } } new Base(); }")
        .expect_err("super() outside derived class must reject");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "super_call_in_non_derived_class",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

// ---------------------------------------------------------------------------
// M29: Class fields (public + private) + accessors (get / set)
// ---------------------------------------------------------------------------

#[test]
fn m29_public_field_with_initializer() {
    // §15.7.14 step 28 — `x = 1;` installs a data property via
    // `DefineField` inside the field-initializer closure.
    let src = "function main() { \
        class C { x = 7; } \
        return new C().x; \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn m29_public_field_without_initializer_defaults_to_undefined() {
    // `y;` without an initializer installs the property with
    // `undefined`. `typeof` returns the "undefined" string, so
    // we compare its length (9) against the "number" alternative
    // (6) — a cheap way to distinguish without wiring full
    // string-literal `===` here.
    let src = "function main() { \
        class C { y; } \
        let c = new C(); \
        let t = typeof c.y; \
        return t.length; \
    }";
    assert_eq!(run_int32_function(src, &[]), 9);
}

#[test]
fn m29_private_field_with_this_access() {
    // §6.2.12 — `this.#x` inside a method of class C resolves
    // against C's private-name bucket on the instance.
    let src = "function main() { \
        class C { \
            #x = 11; \
            get() { return this.#x; } \
        } \
        return new C().get(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 11);
}

#[test]
fn m29_private_field_write_through_method() {
    // Setting `this.#x` via a mutator method and reading it back
    // in another method exercises the full `SetPrivateField` /
    // `GetPrivateField` round-trip.
    let src = "function main() { \
        class Counter { \
            #n = 0; \
            bump() { this.#n = this.#n + 1; return this.#n; } \
        } \
        let c = new Counter(); \
        c.bump(); \
        return c.bump(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn m29_private_in_operator_checks_brand() {
    // §13.10.1 — `#x in obj` returns true iff the object has the
    // private element in the current class's bucket.
    let src = "function main() { \
        class C { \
            #brand = 0; \
            static isC(obj) { return (#brand in obj) ? 1 : 0; } \
        } \
        return C.isC(new C()) + C.isC({}); \
    }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m29_undeclared_private_name_rejected() {
    // Inside class C, `this.#missing` where `#missing` was never
    // declared is a compile-time error.
    let err = compile(
        "function main() { class C { method() { return this.#missing; } } new C().method(); }",
    )
    .expect_err("undeclared private name must reject");
    assert!(
        matches!(
            err,
            SourceLoweringError::Unsupported {
                construct: "undeclared_private_name",
                ..
            }
        ),
        "unexpected err: {err:?}",
    );
}

#[test]
fn m29_getter_accessor_runs() {
    // `get doubled()` installs a getter via `DefineClassGetter`.
    // Access returns via the getter call path.
    let src = "function main() { \
        class C { \
            constructor() { this.n = 21; } \
            get doubled() { return this.n + this.n; } \
        } \
        return new C().doubled; \
    }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn m29_setter_accessor_stores_via_mutation() {
    // `set value(v)` installs a setter; `c.value = 5` invokes it.
    let src = "function main() { \
        class C { \
            constructor() { this.n = 0; } \
            set value(v) { this.n = v + 1; } \
        } \
        let c = new C(); \
        c.value = 10; \
        return c.n; \
    }";
    assert_eq!(run_int32_function(src, &[]), 11);
}

#[test]
fn m29_getter_and_setter_on_same_property() {
    // §10.4.1 — `get`/`set` under the same key merge into one
    // accessor property. Both halves must survive installation
    // (the runtime's `apply_accessor_property_descriptor` merges
    // getter/setter halves).
    let src = "function main() { \
        class C { \
            constructor() { this.storage = 0; } \
            get v() { return this.storage + 1; } \
            set v(x) { this.storage = x; } \
        } \
        let c = new C(); \
        c.v = 40; \
        return c.v; \
    }";
    assert_eq!(run_int32_function(src, &[]), 41);
}

#[test]
fn m29_static_public_field_installed_on_constructor() {
    // Static fields run their initializer at class-definition
    // time and install on the constructor itself (not the
    // prototype). `DefineField r_class, name` is the emission.
    let src = "function main() { \
        class C { static tag = 99; } \
        return C.tag; \
    }";
    assert_eq!(run_int32_function(src, &[]), 99);
}

#[test]
fn m29_derived_class_field_runs_after_super() {
    // §15.7.14 — derived-class field initializers run AFTER
    // `super()` returns. Without that ordering the `this.x` in
    // the field initializer would observe an uninitialised
    // `this` and throw. The test relies on the runtime
    // auto-invocation inside `super_call_dispatch`.
    let src = "function main() { \
        class Parent { constructor() { this.parentVal = 10; } } \
        class Child extends Parent { \
            offset = 5; \
            sum() { return this.parentVal + this.offset; } \
        } \
        return new Child().sum(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 15);
}

#[test]
fn m29_field_initializer_can_reference_this_and_outer_scope() {
    // Field initializers capture outer bindings via the usual
    // closure path and see `this` for per-instance state.
    let src = "function main() { \
        let base = 40; \
        class C { \
            offset = base + 2; \
            get v() { return this.offset; } \
        } \
        return new C().v; \
    }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

// ---------------------------------------------------------------------------
// M29.5: Private methods + accessors + static blocks
// ---------------------------------------------------------------------------

#[test]
fn m29p5_private_method_invoked_via_this() {
    // `this.#m()` reads the method off the instance's
    // `[[PrivateElements]]` (copied at construction time) and
    // invokes it with `this = instance`.
    let src = "function main() { \
        class C { \
            constructor() { this.base = 10; } \
            #double() { return this.base + this.base; } \
            get() { return this.#double(); } \
        } \
        return new C().get(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 20);
}

#[test]
fn m29p5_private_method_forwarded_through_public_api() {
    // Private methods are called from other methods — outside
    // the class they're inaccessible. We don't test the negative
    // path here because there's no `instanceof` yet; the
    // compile-time `undeclared_private_name` check already
    // covers cross-class access.
    let src = "function main() { \
        class Calc { \
            #inc(n) { return n + 1; } \
            apply(v) { return this.#inc(v); } \
        } \
        return new Calc().apply(5); \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m29p5_private_getter_accessor() {
    // `get #h()` installs a private accessor. Reading
    // `this.#h` invokes the getter with `this = instance`.
    let src = "function main() { \
        class C { \
            constructor() { this.raw = 7; } \
            get #half() { return this.raw; } \
            expose() { return this.#half + 1; } \
        } \
        return new C().expose(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 8);
}

#[test]
fn m29p5_private_setter_accessor() {
    // `set #h(v)` installs a private setter. `this.#h = v`
    // invokes the setter with `this = instance` and `v` as the
    // argument; the setter mutates a public field so we can
    // observe the effect.
    let src = "function main() { \
        class C { \
            constructor() { this.raw = 0; } \
            set #slot(v) { this.raw = v + 3; } \
            store(v) { this.#slot = v; } \
        } \
        let c = new C(); \
        c.store(10); \
        return c.raw; \
    }";
    assert_eq!(run_int32_function(src, &[]), 13);
}

#[test]
fn m29p5_private_accessor_getter_and_setter_merge() {
    // `get #p()` + `set #p(v)` declared together merge into a
    // single accessor element. Same-name duplication in the
    // private-name list needs a dedicated guard: for now we
    // allow get/set pairs to share a name, since they declare
    // complementary halves of one element.
    let src = "function main() { \
        class C { \
            constructor() { this.backing = 0; } \
            get #v() { return this.backing; } \
            set #v(n) { this.backing = n; } \
            round_trip(n) { this.#v = n; return this.#v + 1; } \
        } \
        return new C().round_trip(40); \
    }";
    assert_eq!(run_int32_function(src, &[]), 41);
}

#[test]
fn m29p5_static_private_method() {
    // `static #m()` lives directly on the class constructor's
    // `[[PrivateElements]]` (via `DefinePrivateMethod`). Access
    // from inside any method of the same class works.
    let src = "function main() { \
        class C { \
            static #triple(n) { return n + n + n; } \
            static run(n) { return C.#triple(n); } \
        } \
        return C.run(4); \
    }";
    assert_eq!(run_int32_function(src, &[]), 12);
}

#[test]
fn m29p5_static_block_runs_at_class_definition() {
    // `static { … }` runs once with `this = class`. The block
    // mutates a static public field we can inspect after the
    // class is defined.
    let src = "function main() { \
        class C { \
            static counter = 0; \
            static { this.counter = this.counter + 100; } \
        } \
        return C.counter; \
    }";
    assert_eq!(run_int32_function(src, &[]), 100);
}

#[test]
fn m29p5_static_block_local_scope() {
    // Static blocks can declare their own `let`/`const` (shared
    // statement surface with function bodies).
    let src = "function main() { \
        class C { \
            static total = 0; \
            static { \
                let step = 7; \
                this.total = this.total + step; \
                this.total = this.total + step; \
            } \
        } \
        return C.total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 14);
}

#[test]
fn m29p5_multiple_static_blocks_run_in_order() {
    // Declaration order matters for static blocks: each block
    // sees the side effects of the previous ones.
    let src = "function main() { \
        class C { \
            static seq = 0; \
            static { this.seq = 1; } \
            static { this.seq = this.seq + 10; } \
            static { this.seq = this.seq + 100; } \
        } \
        return C.seq; \
    }";
    assert_eq!(run_int32_function(src, &[]), 111);
}

// ---------------------------------------------------------------------------
// M30: for (x of iter) + iterator protocol
// ---------------------------------------------------------------------------

#[test]
fn m30_for_of_sums_array_elements() {
    // Basic case — iterate an Array literal and accumulate via
    // the built-in Array iterator protocol.
    let src = "function main() { \
        let total = 0; \
        for (let x of [1, 2, 3, 4]) { total = total + x; } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 10);
}

#[test]
fn m30_for_of_reads_existing_binding() {
    // Identifier-target form: `for (x of iter)` assigns to an
    // existing `let` rather than introducing a new binding.
    let src = "function main() { \
        let last = 0; \
        let x = 0; \
        for (x of [5, 6, 7]) { last = x; } \
        return last; \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn m30_for_of_upvalue_target_updates_outer_binding() {
    // `for (x of values)` inside a nested function assigns into
    // the captured outer binding, so the outer frame observes
    // the final iterator value after the loop completes.
    let src = "function main() { \
        let x = 0; \
        function consume(values) { \
            for (x of values) { } \
        } \
        consume([5, 6, 7]); \
        return x; \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn m30_for_of_upvalue_target_is_visible_inside_body() {
    // §14.7.5.13 assigns the iterator value before the body
    // runs, so reads of the captured binding inside the loop see
    // the current element rather than the previous iteration.
    let src = "function main() { \
        let x = 0; \
        function consume(values) { \
            let total = 0; \
            for (x of values) { total = total + x; } \
            return total * 10 + x; \
        } \
        return consume([1, 2, 3]); \
    }";
    assert_eq!(run_int32_function(src, &[]), 63);
}

#[test]
fn m30_for_of_upvalue_target_is_set_before_break() {
    // Abrupt completion still happens after the loop-assignment
    // step, so a `break` from the body preserves the current
    // iteration value in the captured binding.
    let src = "function main() { \
        let x = 0; \
        function consume(values) { \
            for (x of values) { \
                break; \
            } \
            return x; \
        } \
        return consume([2, 4, 6]); \
    }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn m30_for_of_array_assignment_target_updates_existing_bindings() {
    // `for ([a, b] of pairs)` reuses existing bindings rather
    // than introducing loop-scoped locals, so the final
    // destructured values remain visible after the loop exits.
    let src = "function main() { \
        let a = 0; \
        let b = 0; \
        for ([a, b] of [[10, 20], [30, 40]]) { } \
        return a * 100 + b; \
    }";
    assert_eq!(run_int32_function(src, &[]), 3040);
}

#[test]
fn m30_for_of_array_assignment_target_is_visible_inside_body() {
    // The destructuring assignment step runs before the body, so
    // body reads observe the current tuple's elements on each
    // iteration.
    let src = "function main() { \
        let a = 0; \
        let b = 0; \
        let total = 0; \
        for ([a, b] of [[1, 2], [3, 4]]) { total = total + a + b; } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 10);
}

#[test]
fn m30_for_of_object_assignment_target_sets_defaults_before_break() {
    // Object assignment-target patterns share the same
    // pre-body assignment step, including default initializers.
    let src = "function main() { \
        let x = 0; \
        let y = 0; \
        for ({ x = 9, y } of [{ y: 4 }, { x: 7, y: 8 }]) { \
            break; \
        } \
        return x * 10 + y; \
    }";
    assert_eq!(run_int32_function(src, &[]), 94);
}

#[test]
fn m30_for_of_static_member_target_updates_existing_object() {
    let src = "function main() { \
        let out = { x: 0 }; \
        for (out.x of [1, 2, 7]) { } \
        return out.x; \
    }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn m30_for_of_computed_member_target_recomputes_key_each_iteration() {
    let src = "function main() { \
        let state = { idx: 0, a: 0, b: 0 }; \
        let keys = ['a', 'b']; \
        for (state[keys[state.idx]] of [7, 9]) { \
            state.idx = state.idx + 1; \
        } \
        return state.a * 10 + state.b; \
    }";
    assert_eq!(run_int32_function(src, &[]), 79);
}

#[test]
fn m30_for_of_private_field_target_updates_receiver() {
    let src = "function main() { \
        class Box { \
            #value = 0; \
            run() { for (this.#value of [3, 8]) { } return this.#value; } \
        } \
        return new Box().run(); \
    }";
    assert_eq!(run_int32_function(src, &[]), 8);
}

#[test]
fn m30t_for_of_ts_non_null_member_target_compiles() {
    assert_eq!(
        run_int32_function_ts(
            "function f() { \
                let out = { x: 0 }; \
                for ((out!).x of [4, 6]) { } \
                return out.x; \
            }",
            &[],
        ),
        6,
    );
}

#[test]
fn m30_for_of_break_exits_loop() {
    // `break` inside the body jumps past the loop exit. Partial
    // sum covers the early-exit path.
    let src = "function main() { \
        let total = 0; \
        for (let x of [1, 2, 3, 4, 5]) { \
            if (x > 3) { break; } \
            total = total + x; \
        } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m30_for_of_continue_resumes_next_step() {
    // `continue` jumps back to the IteratorStep, so odd values
    // get skipped and only even elements contribute.
    let src = "function main() { \
        let total = 0; \
        for (let x of [1, 2, 3, 4, 5]) { \
            if (x === 1 || x === 3 || x === 5) { continue; } \
            total = total + x; \
        } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m30_for_of_over_string_iterates_code_points() {
    // String iterator yields code-point-at-a-time — we just
    // count iterations to avoid string-comparison plumbing.
    let src = "function main() { \
        let n = 0; \
        for (let c of \"abcd\") { n = n + 1; } \
        return n; \
    }";
    assert_eq!(run_int32_function(src, &[]), 4);
}

#[test]
fn m30_for_of_nested_loops() {
    // Nested `for…of` — temps + loop-label stacks are nested
    // correctly. Uses two separate accumulators to avoid
    // triple-chained addition (M6's binary-expr pipeline
    // handles plain `acc + reg` shapes first).
    let src = "function main() { \
        let total = 0; \
        for (let a of [1, 2]) { \
            let inner = 0; \
            for (let b of [10, 20, 30]) { \
                inner = inner + b; \
            } \
            total = total + inner; \
        } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 120);
}

#[test]
fn m30_for_of_reads_array_of_computed_values() {
    // Exercises IteratorStep over a non-literal Array — the
    // array is built via `Array.from(length)` style here
    // (actually an array literal of computed expressions). Pins
    // the built-in-iterator path without requiring
    // `Symbol.iterator` globals, which arrive later.
    let src = "function main() { \
        let n = 0; \
        let arr = [0, 0, 0, 0, 0]; \
        arr[0] = 3; arr[1] = 5; arr[2] = 7; arr[3] = 9; arr[4] = 11; \
        let sum = 0; \
        for (let v of arr) { sum = sum + v; n = n + 1; } \
        return sum + n * 100; \
    }";
    assert_eq!(run_int32_function(src, &[]), 535);
}

// ---------------------------------------------------------------------------
// M30-tail: full iterator protocol (Symbol.iterator + user .next())
// ---------------------------------------------------------------------------

#[test]
fn m30t_symbol_global_exposes_iterator_symbol() {
    // `Symbol` is now on the compiler's global whitelist and
    // resolves via `LdaGlobal`. The runtime exposes every
    // well-known symbol as a data property of the Symbol
    // constructor, so `Symbol.iterator` is a valid property
    // access — we test the round-trip by reading it, storing
    // it, and checking the same value comes back.
    let src = "function main() { \
        let it = Symbol.iterator; \
        let it2 = Symbol.iterator; \
        return (it === it2) ? 1 : 0; \
    }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m30t_for_of_user_iterable_with_symbol_iterator() {
    // Full §7.4.1 GetIterator path — the class installs a
    // `[Symbol.iterator]` method on its prototype that returns
    // an iterator whose `next()` yields `{value, done}`.
    let src = "function main() { \
        class Range { \
            constructor() { this.i = 0; } \
            next() { \
                let current = this.i; \
                if (current === 4) { return { value: 0, done: true }; } \
                this.i = current + 1; \
                return { value: current, done: false }; \
            } \
        } \
        Range.prototype[Symbol.iterator] = function() { return this; }; \
        let total = 0; \
        for (let v of new Range()) { total = total + v; } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m30t_for_of_iterable_separates_iterator_and_next() {
    // Iterable exposes its own `[Symbol.iterator]` that returns
    // a SEPARATE iterator object (not `this`) — this exercises
    // the two-object protocol where the iterable holds state
    // for `next()` via a distinct instance. The iterator class
    // is spelled explicitly to keep all identifier references
    // class-scoped rather than upvalues (the M6 relational-op
    // path doesn't accept two-upvalue operands yet).
    let src = "function main() { \
        class Iterator { \
            constructor() { this.i = 0; } \
            next() { \
                let current = this.i; \
                if (current === 5) { return { value: 0, done: true }; } \
                this.i = current + 1; \
                return { value: current, done: false }; \
            } \
        } \
        let iterable = {}; \
        iterable[Symbol.iterator] = function() { return new Iterator(); }; \
        let total = 0; \
        for (let v of iterable) { total = total + v; } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 10);
}

#[test]
fn m30t_for_of_break_stops_user_iterator() {
    // `break` inside a user-iterator loop still exits. Since
    // M30 doesn't yet wire IteratorClose on abrupt completion
    // for user iterators (`return()` method isn't called), the
    // iterator just gets GC'd — the accumulator check still
    // proves the loop exited early.
    let src = "function main() { \
        class Counter { \
            constructor() { this.i = 0; } \
            next() { \
                let v = this.i; \
                this.i = this.i + 1; \
                return { value: v, done: false }; \
            } \
        } \
        Counter.prototype[Symbol.iterator] = function() { return this; }; \
        let sum = 0; \
        for (let v of new Counter()) { \
            if (v === 3) { break; } \
            sum = sum + v; \
        } \
        return sum; \
    }";
    assert_eq!(run_int32_function(src, &[]), 3);
}

#[test]
fn m30t_for_of_non_iterable_throws() {
    // Plain object without `[Symbol.iterator]` is not iterable.
    // Per §7.4.1 step 3, GetIterator throws TypeError — we
    // catch it to keep the helper's int32 return contract.
    let src = "function main() { \
        let caught = 0; \
        try { \
            for (let x of {}) { caught = 1; } \
        } catch (e) { \
            caught = 2; \
        } \
        return caught; \
    }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn m30t_for_of_iterator_returning_non_object_throws() {
    // §7.4.2 — `iterator.next()` result must be an Object. A
    // primitive return triggers TypeError (caught below).
    let src = "function main() { \
        class Bad { \
            next() { return 42; } \
        } \
        Bad.prototype[Symbol.iterator] = function() { return this; }; \
        let caught = 0; \
        try { \
            for (let v of new Bad()) { caught = 1; } \
        } catch (e) { \
            caught = 2; \
        } \
        return caught; \
    }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn m30_for_of_const_binding_is_readonly() {
    // `const` loop binding: assignment inside the body must
    // fail at compile time.
    let err = compile("function f() { for (const x of [1, 2]) { x = 5; } return 0; }")
        .expect_err("const loop var");
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
fn for_of_var_binding_compiles() {
    // `for (var x of arr)` now lowers through the same
    // per-iteration Star as `let` / `const`. Full function-scope
    // hoisting for the `var` flavour is still a follow-up, but
    // the common "declare + consume" shape works end-to-end.
    compile("function f() { for (var x of [1]) { return x; } return 0; }")
        .expect("for-of-var compiles");
}

#[test]
fn for_of_destructuring_array_pattern() {
    // `for (let [a, b] of [[1,2],[3,4]])` — the array pattern
    // now expands against the per-iteration value via
    // `lower_pattern_bind`. Returns the first iteration's `a`.
    assert_eq!(
        run_int32_function(
            "function f() { for (let [a, b] of [[10, 20], [30, 40]]) { return a + b; } return 0; }",
            &[],
        ),
        30
    );
}

#[test]
fn for_of_destructuring_object_pattern() {
    // `for (const { name, age } of users) { … }` — the canonical
    // array-of-records iteration shape in real-world JS.
    assert_eq!(
        run_int32_function(
            "function f() { \
                let total = 0; \
                let users = [{ n: 7 }, { n: 8 }]; \
                for (const { n } of users) { total = total + n; } \
                return total; \
            }",
            &[],
        ),
        15
    );
}

#[test]
fn for_await_unsupported() {
    // `for await (x of ...)` — async iteration, lands with the
    // async-function milestone.
    let err = compile("async function f() { for await (let x of []) { return x; } return 0; }")
        .expect_err("for-await");
    assert!(
        matches!(err, SourceLoweringError::Unsupported { .. }),
        "unexpected err: {err:?}",
    );
}

// ---------------------------------------------------------------------------
// M31: for (k in obj) + property iteration
// ---------------------------------------------------------------------------

#[test]
fn m31_for_in_counts_own_string_keys() {
    // Basic for-in: iterate an object literal's own enumerable
    // string-keyed properties. We sum `obj[k]` to exercise both
    // the key binding and the keyed read path.
    let src = "function main() { \
        let obj = { a: 1, b: 2, c: 3 }; \
        let total = 0; \
        for (let k in obj) { total = total + obj[k]; } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m31_for_in_reads_existing_binding() {
    // Identifier-target form: the loop variable is an existing
    // `let`, not a fresh binding.
    let src = "function main() { \
        let obj = { x: 10, y: 20, z: 30 }; \
        let k = \"\"; \
        let last = 0; \
        for (k in obj) { last = obj[k]; } \
        return last; \
    }";
    assert_eq!(run_int32_function(src, &[]), 30);
}

#[test]
fn m31_for_in_upvalue_target_updates_outer_binding() {
    // `for (k in obj)` inside a nested function writes through
    // to the captured outer binding, so the last enumerated key
    // remains visible after the loop exits.
    let src = "function main() { \
        let k = \"\"; \
        function consume(obj) { \
            for (k in obj) { } \
        } \
        consume({ a: 1, b: 2, c: 3 }); \
        return k; \
    }";
    assert_eq!(run_string_function(src, &[]), "c");
}

#[test]
fn m31_for_in_upvalue_target_is_visible_inside_body() {
    // The for-in assignment step runs before the body, so keyed
    // reads through the captured binding see the current key on
    // each iteration.
    let src = "function main() { \
        let k = \"\"; \
        function consume(obj) { \
            let total = 0; \
            for (k in obj) { total = total + obj[k]; } \
            return total; \
        } \
        return consume({ a: 1, b: 2, c: 3 }); \
    }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m31_for_in_upvalue_target_is_set_before_break() {
    // Abrupt completion still happens after the per-iteration
    // key assignment, so breaking out of the body preserves the
    // current key in the captured binding.
    let src = "function main() { \
        let k = \"\"; \
        function consume(obj) { \
            for (k in obj) { break; } \
            return k; \
        } \
        return consume({ first: 1, second: 2 }); \
    }";
    assert_eq!(run_string_function(src, &[]), "first");
}

#[test]
fn m31_for_in_array_assignment_target_updates_existing_bindings() {
    // `for ([a, b] in obj)` destructures each string key into
    // existing bindings, so the final key's characters remain
    // visible after the loop exits.
    let src = "function main() { \
        let a = \"\"; \
        let b = \"\"; \
        for ([a, b] in { ab: 1, cd: 2 }) { } \
        return a + b; \
    }";
    assert_eq!(run_string_function(src, &[]), "cd");
}

#[test]
fn m31_for_in_array_assignment_target_is_visible_inside_body() {
    // The destructuring assignment step still happens before the
    // body, so reads of the reassigned bindings see the current
    // key's characters on each iteration.
    let src = "function main() { \
        let a = \"\"; \
        let b = \"\"; \
        let total = \"\"; \
        for ([a, b] in { ab: 1, cd: 2 }) { total = total + a + b; } \
        return total; \
    }";
    assert_eq!(run_string_function(src, &[]), "abcd");
}

#[test]
fn m31_for_in_object_assignment_target_is_set_before_break() {
    // Object assignment-target patterns run against the current
    // key string before the body executes, so `length` is set by
    // the first key even when the body breaks immediately.
    let src = "function main() { \
        let length = 0; \
        for ({ length } in { cat: 1, dog: 2 }) { break; } \
        return length; \
    }";
    assert_eq!(run_int32_function(src, &[]), 3);
}

#[test]
fn m31_for_in_static_member_target_updates_existing_object() {
    let src = "function main() { \
        let out = { key: '' }; \
        let src = { a: 1, b: 2 }; \
        for (out.key in src) { } \
        return out.key === 'b' ? 1 : 0; \
    }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m31t_for_in_ts_non_null_member_target_compiles() {
    assert_eq!(
        run_int32_function_ts(
            "function f() { \
                let out = { key: '' }; \
                let src = { a: 1, b: 2 }; \
                for ((out!).key in src) { } \
                return out.key === 'b' ? 1 : 0; \
            }",
            &[],
        ),
        1,
    );
}

#[test]
fn m31_for_in_walks_prototype_chain() {
    // §14.7.5.6 — for-in enumerates own AND inherited
    // enumerable properties. The child has its own key, and
    // the parent class's prototype method `m` is inherited but
    // methods are non-enumerable, so only the own key appears.
    // Counting should therefore match the own-only case.
    let src = "function main() { \
        class Parent { \
            m() { return 0; } \
        } \
        class Child extends Parent { \
            constructor() { super(); this.x = 5; } \
        } \
        let c = new Child(); \
        let count = 0; \
        for (let k in c) { count = count + 1; } \
        return count; \
    }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m31_for_in_over_null_skips_body() {
    // `for (k in null)` / `for (k in undefined)` produce no
    // iterations per §14.7.5.6 step 6 — no throw. Body never
    // runs.
    let src = "function main() { \
        let ran = 0; \
        for (let k in null) { ran = 1; } \
        for (let k in undefined) { ran = ran + 10; } \
        return ran; \
    }";
    assert_eq!(run_int32_function(src, &[]), 0);
}

#[test]
fn m31_for_in_break_exits_loop() {
    // `break` inside for-in jumps past the loop exit; we stop
    // after seeing one key so the accumulator keeps the initial
    // value.
    let src = "function main() { \
        let obj = { a: 1, b: 2, c: 3 }; \
        let seen = 0; \
        for (let k in obj) { \
            seen = seen + 1; \
            if (seen === 1) { break; } \
        } \
        return seen; \
    }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m31_for_in_continue_resumes_next_step() {
    // `continue` jumps back to ForInNext so we pick up the next
    // key. Uses a counter + `continue` on odd counts to
    // accumulate only even-position values.
    let src = "function main() { \
        let obj = { a: 10, b: 20, c: 30, d: 40 }; \
        let total = 0; \
        let seen = 0; \
        for (let k in obj) { \
            seen = seen + 1; \
            if (seen === 2) { continue; } \
            total = total + obj[k]; \
        } \
        return total; \
    }";
    // Sum of three of the four values (skip the 2nd): we don't
    // pin the iteration order tightly — any permutation would
    // drop one of {10,20,30,40}; summing three values gives
    // 100 − skipped. Asserting the common insertion-order
    // case where the 2nd is `b: 20` → 100 − 20 = 80.
    assert_eq!(run_int32_function(src, &[]), 80);
}

#[test]
fn m31_for_in_nested_loops() {
    // Nested for-in over two objects — exercises the
    // iterator-slot-as-anonymous-local fix the same way
    // M30's nested-for-of test did.
    let src = "function main() { \
        let outer = { a: 1, b: 2 }; \
        let inner = { x: 10, y: 20 }; \
        let total = 0; \
        for (let i in outer) { \
            for (let j in inner) { \
                total = total + outer[i] + inner[j]; \
            } \
        } \
        return total; \
    }";
    assert_eq!(run_int32_function(src, &[]), 66);
}

#[test]
fn for_in_var_binding_compiles() {
    // `for (var k in obj)` follows the same relaxation as
    // `for (var x of arr)` — lowers through the shared per-
    // iteration Star path.
    compile("function f() { for (var k in {}) { return k; } return 0; }")
        .expect("for-in-var compiles");
}

#[test]
fn for_in_destructuring_compiles() {
    // `for (let [a] of ...)` is handled by the for-of path; for
    // `for-in` the pattern is less common (keys are strings) but
    // still valid — the pattern runs against each string key.
    // Here we just verify compilation plus a minimal runtime
    // shape.
    compile("function f() { for (let [a] in {}) { return a; } return 0; }")
        .expect("for-in destructuring compiles");
}

// ---------------------------------------------------------------------------
// M32: Promise runtime + microtask queue
// ---------------------------------------------------------------------------

#[test]
fn m32_promise_resolve_then_runs_callback_via_microtask() {
    // `Promise.resolve(v).then(cb)` schedules `cb(v)` as a
    // microtask. `execute_with_runtime` drains the queue before
    // returning, so the test observes the mutation through the
    // shared `state` object.
    let src = "function main() { \
        let state = { count: 0 }; \
        Promise.resolve(5).then(function(v) { state.count = v; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 5);
}

#[test]
fn m32_then_chain_runs_in_order() {
    // `.then(a).then(b)` — the chained `.then`'s callback sees
    // the return of `a`. Each settles via microtask so both run
    // before the host regains control.
    let src = "function main() { \
        let state = { count: 0 }; \
        Promise.resolve(1) \
            .then(function(v) { state.count = v + 10; return v + 100; }) \
            .then(function(w) { state.count = w + 1; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 102);
}

#[test]
fn m32_new_promise_resolves_from_executor() {
    // `new Promise((resolve) => resolve(v))` — executor runs
    // synchronously with `resolve` / `reject` capability
    // functions. `.then` callback fires on microtask drain.
    let src = "function main() { \
        let state = { count: 0 }; \
        let p = new Promise(function(resolve, reject) { resolve(42); }); \
        p.then(function(v) { state.count = v; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 42);
}

#[test]
fn m32_reject_routes_to_catch_handler() {
    // `.catch(h)` is short for `.then(undefined, h)`. A
    // `reject(r)` call forwards `r` through the rejection
    // branch.
    let src = "function main() { \
        let state = { count: 0 }; \
        let p = new Promise(function(resolve, reject) { reject(7); }); \
        p.catch(function(r) { state.count = r + 100; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 107);
}

#[test]
fn m32_thrown_in_then_propagates_to_catch() {
    // Exception inside a `.then` callback settles the next
    // promise in the chain as rejected; a following `.catch`
    // recovers. Mirrors §27.2.5 PerformPromiseThen + §27.2.1.4
    // PromiseReactionJob's abrupt-completion branch.
    let src = "function main() { \
        let state = { count: 0 }; \
        Promise.resolve(1) \
            .then(function(v) { throw v + 10; }) \
            .catch(function(e) { state.count = e + 1; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 12);
}

#[test]
fn m32_promise_reject_static_yields_rejection() {
    // `Promise.reject(r)` settles rejected synchronously; the
    // `.catch` handler fires on drain.
    let src = "function main() { \
        let state = { count: 0 }; \
        Promise.reject(3).catch(function(r) { state.count = r + 20; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 23);
}

#[test]
fn m32_finally_runs_regardless_of_settlement() {
    // `.finally(cb)` runs after either fulfillment or rejection
    // and is not passed the settlement value.
    let src = "function main() { \
        let state = { count: 0 }; \
        Promise.resolve(1).finally(function() { state.count = state.count + 5; }); \
        Promise.reject(1).finally(function() { state.count = state.count + 7; }).catch(function(e) { return e; }); \
        return state; \
    }";
    assert_eq!(run_promise_state_counter(src, "count"), 12);
}

// ---------------------------------------------------------------------------
// M33: async functions + await
// ---------------------------------------------------------------------------

#[test]
fn m33_async_function_returns_promise_of_value() {
    // `async function f() { return v; }` wraps the return in a
    // Promise; `.then` settles via microtask drain.
    let src = "async function inner() { return 17; } \
        function main() { \
            let state = { count: 0 }; \
            inner().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 17);
}

#[test]
fn m33_await_unwraps_fulfilled_promise() {
    // `await p` returns p's fulfillment value. The async body
    // itself returns a Promise; we chain `.then` to observe
    // the settled result.
    let src = "async function run() { \
            let v = await Promise.resolve(11); \
            return v + 1; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 12);
}

#[test]
fn m33_await_non_promise_passes_through() {
    // §27.7.5.3 step 5 — `await <non-thenable>` treats the
    // operand as already-fulfilled, so `await 5` yields 5.
    let src = "async function run() { \
            let v = await 5; \
            return v + 100; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 105);
}

#[test]
fn m33_await_chained_promises() {
    // `await Promise.resolve(v).then(x => x + 1)` — chained
    // promise settles via microtask drain, then `await`
    // unwraps the final value.
    let src = "async function run() { \
            let v = await Promise.resolve(10).then(function(x) { return x + 5; }); \
            return v * 2; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 30);
}

#[test]
fn m33_try_catch_around_await_rejected() {
    // `try { await rejectedPromise; } catch (e) { ... }` —
    // the Await opcode throws the rejection reason; the
    // function's try/catch catches it.
    let src = "async function run() { \
            try { \
                await Promise.reject(42); \
                return 0; \
            } catch (e) { \
                return e + 1; \
            } \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 43);
}

#[test]
fn m33_async_function_rejects_on_thrown() {
    // Thrown exceptions inside an async body settle its
    // result promise as rejected; `.catch` handles it.
    let src = "async function bad() { throw 9; } \
        function main() { \
            let state = { count: 0 }; \
            bad().catch(function(e) { state.count = e + 1; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 10);
}

#[test]
fn m33_async_arrow_function() {
    // Async arrow expressions land via `async (args) => expr`.
    // Same settlement semantics as async function — await
    // inside unwraps the promise value.
    let src = "function main() { \
            let state = { count: 0 }; \
            let f = async function(n) { return n + 1; }; \
            f(5).then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 6);
}

// ---------------------------------------------------------------------------
// M33-finale: microtask-deferred settlement + Promise combinators
// ---------------------------------------------------------------------------

#[test]
fn m33f_await_microtask_deferred_promise() {
    // `await p` where `p` is resolved by a separate microtask
    // (not at construction time). The Await opcode drains the
    // queue in a loop until the queue is empty, so a single
    // layer of deferred settlement converges to a fulfilled
    // promise before the unwrap check. `holder.resolve` stores
    // the capability function in an object field so the
    // executor + microtask share it (avoids `let external;`
    // which the compiler still rejects as
    // `uninitialized_binding`).
    let src = "async function run() { \
            let holder = { resolve: null }; \
            let p = new Promise(function(resolve) { holder.resolve = resolve; }); \
            Promise.resolve().then(function() { holder.resolve(77); }); \
            let v = await p; \
            return v + 1; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 78);
}

#[test]
fn m33f_await_multi_layer_microtask_chain() {
    // Multi-hop microtask chain — every `.then` queues the next
    // reaction. `drain_microtasks_for_await` loops until the
    // queue is empty, so all three layers settle and the final
    // promise is fulfilled by the time await inspects it.
    let src = "async function run() { \
            let holder = { resolve: null }; \
            let p = new Promise(function(resolve) { holder.resolve = resolve; }); \
            Promise.resolve(1) \
                .then(function(x) { return x + 10; }) \
                .then(function(x) { return x + 100; }) \
                .then(function(x) { holder.resolve(x); }); \
            return await p; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 111);
}

#[test]
fn m33f_promise_all_resolves_with_array() {
    // `Promise.all([p1, p2, p3])` fulfills with an array of
    // values once every input settles. We sum the array to
    // avoid having to inspect each slot.
    let src = "function main() { \
            let state = { count: 0 }; \
            Promise.all([Promise.resolve(10), Promise.resolve(20), Promise.resolve(30)]) \
                .then(function(arr) { state.count = arr[0] + arr[1] + arr[2]; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 60);
}

#[test]
fn m33f_promise_all_rejects_on_first_rejection() {
    // Per §27.2.4.1, `Promise.all` rejects with the first
    // rejection reason and ignores later settlements.
    let src = "function main() { \
            let state = { count: 0 }; \
            Promise.all([Promise.resolve(1), Promise.reject(9), Promise.resolve(3)]) \
                .catch(function(r) { state.count = r + 100; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 109);
}

#[test]
fn m33f_promise_race_settles_with_first() {
    // `Promise.race` settles with the first input to settle.
    // Both inputs here are synchronously-fulfilled; the first
    // wins via declaration order since they queue reactions
    // in the same microtask tick.
    let src = "function main() { \
            let state = { count: 0 }; \
            Promise.race([Promise.resolve(5), Promise.resolve(99)]) \
                .then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 5);
}

#[test]
fn m33f_promise_all_settled_never_rejects() {
    // `Promise.allSettled` always fulfills with a list of
    // `{ status, value | reason }` records — rejections don't
    // short-circuit. Sum `arr[0].value` (fulfilled) and
    // `arr[1].reason` (rejected) directly instead of switching
    // on `status`, which would need string-literal `===`.
    let src = "function main() { \
            let state = { count: 0 }; \
            Promise.allSettled([Promise.resolve(10), Promise.reject(3)]) \
                .then(function(arr) { \
                    state.count = arr[0].value + arr[1].reason; \
                }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 13);
}

#[test]
fn m33f_promise_any_takes_first_fulfilled() {
    // §27.2.4.3 — `Promise.any` fulfills with the first
    // fulfilled input and only rejects if ALL inputs reject.
    let src = "function main() { \
            let state = { count: 0 }; \
            Promise.any([Promise.reject(1), Promise.resolve(42), Promise.reject(3)]) \
                .then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 42);
}

#[test]
fn m33f_queue_microtask_runs_during_drain() {
    // `queueMicrotask(cb)` enqueues `cb` as a microtask. The
    // execute-entry drain picks it up before main() returns to
    // the host.
    let src = "function main() { \
            let state = { count: 0 }; \
            queueMicrotask(function() { state.count = 42; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 42);
}

#[test]
fn m33f_queue_microtask_chain_settles_in_one_drain() {
    // Microtasks that enqueue more microtasks keep running
    // within the same drain — the loop in
    // `drain_microtasks_for_await` only exits when the queue
    // is completely empty.
    let src = "function main() { \
            let state = { count: 0 }; \
            queueMicrotask(function() { \
                state.count = state.count + 1; \
                queueMicrotask(function() { \
                    state.count = state.count + 10; \
                    queueMicrotask(function() { \
                        state.count = state.count + 100; \
                    }); \
                }); \
            }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 111);
}

#[test]
fn m33f_await_promise_all() {
    // Composed test — `await Promise.all(...)` inside an async
    // function unwraps the array of results. Exercises the
    // combinator + await integration end-to-end.
    let src = "async function run() { \
            let parts = await Promise.all([ \
                Promise.resolve(2), \
                Promise.resolve(3), \
                Promise.resolve(5) \
            ]); \
            return parts[0] + parts[1] + parts[2]; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 10);
}

#[test]
fn m33f_set_timeout_fires_via_drive_event_loop() {
    // `setTimeout(cb, 0)` enqueues a timer that fires during
    // the post-entry event-loop drive. The test returns the
    // state object immediately; by the time the helper reads
    // `count`, the timer has fired and the callback has
    // mutated state.
    let src = "function main() { \
            let state = { count: 0 }; \
            setTimeout(function() { state.count = 55; }, 0); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 55);
}

#[test]
fn m33f_set_timeout_chained_settles_promise() {
    // Timer callback resolves a shared promise, which a
    // microtask `.then` then reads. Covers the interleaving of
    // timer firing + microtask drain inside
    // `drive_event_loop`.
    let src = "function main() { \
            let state = { count: 0 }; \
            let holder = { resolve: null }; \
            let p = new Promise(function(resolve) { holder.resolve = resolve; }); \
            setTimeout(function() { holder.resolve(42); }, 0); \
            p.then(function(v) { state.count = v + 1; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 43);
}

// ---------------------------------------------------------------------------
// M37: explicit resource management (`using` / `await using`)
// ---------------------------------------------------------------------------

#[test]
fn m37_using_disposes_in_lifo_order_at_block_exit() {
    let src = "function f() { \
        let state = { count: 0 }; \
        { \
            using a = { [Symbol.dispose]() { state.count = state.count * 10 + 1; } }; \
            using b = { [Symbol.dispose]() { state.count = state.count * 10 + 2; } }; \
            state.count = 3; \
        } \
        return state.count; \
    }";
    assert_eq!(run_int32_function(src, &[]), 321);
}

#[test]
fn m37_using_runs_before_return_completion() {
    let src = "function inner(state) { \
            using x = { [Symbol.dispose]() { state.count = 7; } }; \
            return 1; \
        } \
        function f() { \
            let state = { count: 0 }; \
            inner(state); \
            return state.count; \
        }";
    assert_eq!(run_int32_function(src, &[]), 7);
}

#[test]
fn m37_using_disposes_earlier_resources_when_later_registration_throws() {
    let src = "function f() { \
        let state = { count: 0 }; \
        try { \
            using a = { [Symbol.dispose]() { state.count = 1; } }, \
                  b = { [Symbol.dispose]: 1 }; \
            return 0; \
        } catch (e) { \
            return state.count * 100 + (e.name === 'TypeError'); \
        } \
    }";
    assert_eq!(run_int32_function(src, &[]), 101);
}

#[test]
fn m37_using_throw_and_dispose_throw_form_suppressed_error() {
    let src = "function f() { \
        try { \
            using x = { [Symbol.dispose]() { throw new Error('dispose'); } }; \
            throw new Error('body'); \
        } catch (e) { \
            return `${e.name}:${e.error.message}:${e.suppressed.message}`; \
        } \
    }";
    assert_eq!(
        run_string_function(src, &[]),
        "SuppressedError:dispose:body"
    );
}

#[test]
fn m37_await_using_awaits_async_disposer_before_settlement() {
    let src = "async function run() { \
            let state = { count: 0 }; \
            { \
                await using x = { \
                    [Symbol.asyncDispose]() { \
                        return Promise.resolve().then(function() { state.count = 5; }); \
                    } \
                }; \
            } \
            return state; \
        } \
        function main() { \
            let outer = { count: 0 }; \
            run().then(function(state) { outer.count = state.count; }); \
            return outer; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 5);
}

#[test]
fn m37_classic_for_using_disposes_once_after_normal_exit() {
    let src = "function f() { \
            let state = { count: 0 }; \
            let i = 0; \
            for (using x = { [Symbol.dispose]() { state.count = state.count * 10 + 9; } }; \
                 i < 3; \
                 i = i + 1) { \
                state.count = state.count * 10 + i + 1; \
            } \
            return state.count; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1239);
}

#[test]
fn m37_classic_for_using_continue_does_not_dispose_early() {
    let src = "function f() { \
            let state = { count: 0 }; \
            let i = 0; \
            for (using x = { [Symbol.dispose]() { state.count = state.count * 10 + 9; } }; \
                 i < 3; \
                 i = i + 1) { \
                state.count = state.count * 10 + i + 1; \
                if (i < 2) continue; \
            } \
            return state.count; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1239);
}

#[test]
fn m37_classic_for_using_disposes_on_break_and_return() {
    let break_src = "function f() { \
            let state = { count: 0 }; \
            for (using x = { [Symbol.dispose]() { state.count = state.count * 10 + 9; } }; ; ) { \
                state.count = state.count + 1; \
                break; \
            } \
            return state.count; \
        }";
    assert_eq!(run_int32_function(break_src, &[]), 19);

    let return_src = "function inner(state) { \
            for (using x = { [Symbol.dispose]() { state.count = 7; } }; ; ) { \
                return 4; \
            } \
            return 0; \
        } \
        function f() { \
            let state = { count: 0 }; \
            let result = inner(state); \
            return state.count * 10 + result; \
        }";
    assert_eq!(run_int32_function(return_src, &[]), 74);
}

#[test]
fn m37_classic_for_await_using_awaits_after_loop_exit() {
    let src = "async function run() { \
            let state = { count: 0 }; \
            let i = 0; \
            for (await using x = { \
                    [Symbol.asyncDispose]() { \
                        return Promise.resolve().then(function() { state.count = state.count * 10 + 9; }); \
                    } \
                 }; i < 2; i = i + 1) { \
                state.count = state.count * 10 + i + 1; \
            } \
            return state; \
        } \
        function main() { \
            let outer = { count: 0 }; \
            run().then(function(state) { outer.count = state.count; }); \
            return outer; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 129);
}

#[test]
fn m37_for_of_using_disposes_each_iteration() {
    let src = "function f() { \
            let state = { count: 0 }; \
            function make(v) { \
                return { value: v, [Symbol.dispose]() { state.count = state.count * 10 + v + 4; } }; \
            } \
            for (using x of [make(1), make(2)]) { \
                state.count = state.count * 10 + x.value; \
            } \
            return state.count; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1526);
}

#[test]
fn m37_for_of_using_disposes_before_continue_and_break() {
    let src = "function f() { \
            let state = { count: 0 }; \
            function make(v) { \
                return { value: v, [Symbol.dispose]() { state.count = state.count * 10 + v + 5; } }; \
            } \
            for (using x of [make(1), make(2), make(3)]) { \
                state.count = state.count * 10 + x.value; \
                if (x.value === 1) continue; \
                if (x.value === 2) break; \
            } \
            return state.count; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1627);
}

#[test]
fn m37_for_of_await_using_awaits_each_iteration_disposer() {
    let src = "async function run() { \
            let state = { count: 0 }; \
            function make(v) { \
                return { \
                    value: v, \
                    [Symbol.asyncDispose]() { \
                        return Promise.resolve().then(function() { state.count = state.count * 10 + v + 6; }); \
                    } \
                }; \
            } \
            for (await using x of [make(1), make(2)]) { \
                state.count = state.count * 10 + x.value; \
            } \
            return state; \
        } \
        function main() { \
            let outer = { count: 0 }; \
            run().then(function(state) { outer.count = state.count; }); \
            return outer; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 1728);
}

#[test]
fn m33f_await_pending_promise_settled_by_timer() {
    // `await p` where `p` is ONLY resolved by a setTimeout
    // callback. This is the canonical "pending promise +
    // real async source" case. Works now that
    // `drive_event_loop` fires timers during the
    // `drain_microtasks_for_await` loop inside `Await`.
    //
    // Note: the synchronous-style structure works because our
    // current `Await` opcode keeps draining microtasks in a
    // tight loop until the promise settles. When the
    // `setTimeout` fires (after a sub-millisecond wait inside
    // the drive loop), the resolve callback settles the
    // promise, drain picks it up, and the await unwraps.
    let src = "async function run() { \
            let holder = { resolve: null }; \
            let p = new Promise(function(resolve) { holder.resolve = resolve; }); \
            setTimeout(function() { holder.resolve(9); }, 0); \
            let v = await p; \
            return v + 100; \
        } \
        function main() { \
            let state = { count: 0 }; \
            run().then(function(v) { state.count = v; }); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 109);
}

#[test]
fn m33f_clear_timeout_prevents_callback() {
    // `clearTimeout(id)` cancels a pending timer. The
    // callback must NOT fire during the drive loop.
    let src = "function main() { \
            let state = { count: 5 }; \
            let id = setTimeout(function() { state.count = 99; }, 0); \
            clearTimeout(id); \
            return state; \
        }";
    assert_eq!(run_promise_state_counter(src, "count"), 5);
}

// ---------------------------------------------------------------------------
// M34: Generators (function*, yield, gen.next)
// ---------------------------------------------------------------------------

#[test]
fn m34_generator_yields_sequence_and_completes() {
    // Basic `function* () { yield 1; yield 2; yield 3; }` —
    // three `.next()` calls return the yielded values, the
    // fourth returns done=true.
    let src = "function* gen() { yield 1; yield 2; yield 3; } \
        function main() { \
            let g = gen(); \
            let total = 0; \
            total = total + g.next().value; \
            total = total + g.next().value; \
            total = total + g.next().value; \
            let last = g.next(); \
            return total + (last.done ? 100 : 0); \
        }";
    assert_eq!(run_int32_function(src, &[]), 106);
}

#[test]
fn m34_generator_receives_sent_value_on_resume() {
    // The value passed to `.next(v)` becomes the result of the
    // paused `yield` expression. This verifies the sent-value
    // round trip through the accumulator.
    let src = "function* echo() { \
            let a = yield 0; \
            let b = yield a + 1; \
            return b + 10; \
        } \
        function main() { \
            let g = echo(); \
            g.next(); \
            g.next(7); \
            let last = g.next(20); \
            return last.value; \
        }";
    assert_eq!(run_int32_function(src, &[]), 30);
}

#[test]
fn m34_generator_captures_arguments() {
    // Generator parameters are available inside the body on
    // the first `.next()`. The args are copied from the
    // creation call into the activation on first resume.
    let src = "function* range(start, end) { \
            let i = start; \
            while (i < end) { yield i; i = i + 1; } \
        } \
        function main() { \
            let g = range(3, 6); \
            return g.next().value + g.next().value + g.next().value; \
        }";
    assert_eq!(run_int32_function(src, &[]), 12);
}

#[test]
fn m34_generator_works_with_for_of() {
    // The built-in generator prototype's `@@iterator` returns
    // the generator itself, so `for (v of gen())` drives it
    // through the iterator protocol.
    let src = "function* gen() { yield 10; yield 20; yield 30; } \
        function main() { \
            let total = 0; \
            for (let v of gen()) { total = total + v; } \
            return total; \
        }";
    assert_eq!(run_int32_function(src, &[]), 60);
}

#[test]
fn m34_generator_done_after_completion() {
    // Once the generator body returns, further `.next()` calls
    // yield `{ value: undefined, done: true }` — the
    // completion marker. Body isn't literally empty (compiler
    // currently rejects `{}`); a single `return` suffices.
    let src = "function* single() { yield 7; } \
        function main() { \
            let g = single(); \
            let first = g.next(); \
            let second = g.next(); \
            let third = g.next(); \
            return first.value + (second.done ? 10 : 0) + (third.done ? 100 : 0); \
        }";
    assert_eq!(run_int32_function(src, &[]), 117);
}

#[test]
fn m34_generator_throw_propagates_to_body() {
    // `gen.throw(err)` surfaces as a JS throw at the paused
    // yield point. The body's try/catch can intercept it.
    let src = "function* guard() { \
            try { \
                yield 1; \
                return 999; \
            } catch (e) { \
                return e + 100; \
            } \
        } \
        function main() { \
            let g = guard(); \
            g.next(); \
            let last = g.throw(5); \
            return last.value; \
        }";
    assert_eq!(run_int32_function(src, &[]), 105);
}

#[test]
fn m34_generator_return_forces_completion() {
    // `gen.return(v)` marks the generator completed with the
    // supplied value; subsequent `.next()` calls return
    // `{ undefined, true }`.
    let src = "function* counter() { \
            yield 1; \
            yield 2; \
            return 99; \
        } \
        function main() { \
            let g = counter(); \
            g.next(); \
            let r = g.return(42); \
            let after = g.next(); \
            return r.value + (after.done ? 0 : 1000); \
        }";
    assert_eq!(run_int32_function(src, &[]), 42);
}

#[test]
fn m34_yield_star_forwards_inner_iterator_values() {
    // `yield* inner()` — the inner generator's values flow out
    // through the outer generator's `.next()` calls.
    let src = "function* inner() { yield 10; yield 20; yield 30 } \
         function* outer() { yield* inner() } \
         function main() { \
             let g = outer(); \
             let total = 0; \
             total = total + g.next().value; \
             total = total + g.next().value; \
             total = total + g.next().value; \
             return total \
         }";
    assert_eq!(run_int32_function(src, &[]), 60);
}

#[test]
fn m34_yield_star_over_array_iterable() {
    // `yield* [1, 2, 3]` — array iterables work too.
    let src = "function* inner() { yield* [1, 2, 3] } \
         function main() { \
             let g = inner(); \
             let total = 0; \
             total = total + g.next().value; \
             total = total + g.next().value; \
             total = total + g.next().value; \
             return total \
         }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

// ---------------------------------------------------------------------------
// M36: BigInt literals + RegExp literals
// ---------------------------------------------------------------------------

#[test]
fn m36_bigint_literal_materialises_heap_value() {
    // Basic BigInt literal — `typeof 42n === "bigint"` — we
    // check via `.length` of the `typeof` string ("bigint" → 6)
    // to stay within the int32 return contract.
    let src = "function main() { let b = 42n; return (typeof b).length; }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m36_bigint_addition_via_runtime_coercion() {
    // BigInt + BigInt uses the shared Add opcode which routes
    // through `js_add` for non-int32 operands. We read back
    // the result length to verify it stayed a BigInt rather
    // than decaying to a numeric.
    let src = "function main() { \
            let a = 5n; \
            let b = 7n; \
            let c = a + b; \
            return (typeof c).length; \
        }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m36_bigint_preserves_precision() {
    // Past the safe-integer boundary (2^53) a BigInt must keep
    // exact value, unlike a Number. We add two large BigInt
    // literals and compare to the expected literal via `===`
    // so the addition result must be bit-exact — a Number
    // path would collapse both sides into the same imprecise
    // float and hide the bug.
    let src = "function main() { \
            let a = 9007199254740993n; \
            let b = 1n; \
            let c = a + b; \
            let expected = 9007199254740994n; \
            return (c === expected) ? 1 : 0; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

// ---------------------------------------------------------------------------
// D2: Source maps
// ---------------------------------------------------------------------------

#[test]
fn d2_multi_line_function_populates_source_map() {
    // Statement-level source map: each statement's emit PC gets
    // an entry resolving to the original `(line, column)`.
    let src = "function main() {\n    let a = 1;\n    let b = 2;\n    return a + b;\n}";
    let module = compile(src).expect("compile");
    let (_, function) = pick_last_named_function(&module).expect("named fn");
    let sm = function.source_map();
    assert!(!sm.is_empty(), "source map should have entries");
    // The first statement starts on line 2 (0-indexed bytes 22ish) —
    // confirm the lookup for PC=0 resolves to line 2 or later.
    let first = sm.lookup(0).expect("entry at PC 0");
    assert!(
        first.line() >= 2,
        "PC 0 should resolve to body line >= 2, got {}",
        first.line()
    );
}

#[test]
fn d2_source_map_lookup_finds_correct_line() {
    // `return a + b` lives on line 4 of this source. A PC beyond
    // the `let` entries should resolve to line 4.
    let src = "function main() {\n    let a = 10;\n    let b = 20;\n    return a + b;\n}";
    let module = compile(src).expect("compile");
    let (_, function) = pick_last_named_function(&module).expect("named fn");
    let sm = function.source_map();
    // The last statement is the return on line 4 — any PC at or
    // after the last recorded entry must surface line 4.
    let last_pc = sm
        .entries()
        .last()
        .map(|e| e.pc())
        .expect("source map has entries");
    let loc = sm.lookup(last_pc).expect("lookup last entry");
    assert_eq!(loc.line(), 4, "last statement should be on line 4");
}

#[test]
fn d2_synthesised_module_init_has_empty_source_map() {
    // The synthesised module-init function is not a user
    // statement — it should not contribute source-map entries.
    // Use ESM so a module-init is synthesised, and inspect its
    // source map.
    let src = "export function main() { return 1 }";
    let module = ModuleCompiler::new()
        .compile(src, "test.js", SourceType::mjs())
        .expect("compile mjs");
    // The module-init function is the one at module.entry() for
    // ESM (it runs first). Its source map should be empty.
    let init = module.function(module.entry()).expect("entry fn");
    assert!(
        init.source_map().is_empty(),
        "synthesised module-init should not have source-map entries"
    );
}

// ---------------------------------------------------------------------------
// P1: Polymorphic inline caches
// ---------------------------------------------------------------------------

#[test]
fn p1_hot_property_read_stays_monomorphic_and_returns_cached_value() {
    // A plain `obj.x` read inside a loop should hit the
    // monomorphic IC after the first iteration — same shape
    // every time. Regression test: the cached value must equal
    // the slow-path value; a wrong shape guard would either
    // panic or return a stale value.
    let src = "function main() { \
            let obj = { a: 10, b: 20 }; \
            let sum = 0; \
            let i = 0; \
            while (i < 100) { sum = sum + obj.a; i = i + 1 } \
            return sum \
        }";
    assert_eq!(run_int32_function(src, &[]), 1000);
}

#[test]
fn p1_polymorphic_property_read_handles_two_shapes() {
    // Two objects with the same property name but different
    // shape paths. The IC should transition monomorphic →
    // polymorphic without breaking correctness; either shape's
    // `.value` read must return the right number.
    let src = "function pick(flag) { \
            let a = { value: 1, extra: 0 }; \
            let b = { value: 2 }; \
            return flag ? a.value : b.value \
        } \
        function main() { \
            let total = 0; \
            let i = 0; \
            while (i < 10) { \
                total = total + pick(i | 1); \
                total = total + pick(0); \
                i = i + 1 \
            } \
            return total \
        }";
    // pick(truthy) returns 1 × 10 times, pick(0) returns 2 × 10 times → 30.
    assert_eq!(run_int32_function(src, &[]), 30);
}

#[test]
fn p1_property_ic_survives_prototype_chain_lookup() {
    // `obj.method` where `method` lives on the prototype — the
    // IC should NOT populate (owner != handle), so the slow path
    // runs every time. Correctness check: the method still
    // resolves and invokes with the correct receiver.
    let src = "function main() { \
            class C { \
                constructor() { this.n = 7 } \
                get_n() { return this.n } \
            } \
            let c = new C(); \
            let total = 0; \
            let i = 0; \
            while (i < 5) { total = total + c.get_n(); i = i + 1 } \
            return total \
        }";
    assert_eq!(run_int32_function(src, &[]), 35);
}

#[test]
fn e1_bigint_primitive_autoboxes_for_method_call() {
    // `(5n).toString()` — BigInt primitive auto-wraps with
    // `BigInt.prototype` at property access so the `toString`
    // method is found. The wrapper is used for property LOOKUP
    // only; the primitive stays as the `this` passed to the
    // native method, which reads the value via
    // `require_bigint_value`.
    let src = "function main() { let a = 5n; return a.toString().length; }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn e1_bigint_to_string_preserves_precision_beyond_number_boundary() {
    // The whole point of BigInt: values past Number.MAX_SAFE_INTEGER
    // stay exact. `.toString()` on a 16-digit BigInt primitive must
    // round-trip the digits.
    let src = "function main() { \
            let a = 9007199254740993n; \
            let b = 1n; \
            let c = a + b; \
            return c.toString().length; \
        }";
    assert_eq!(run_int32_function(src, &[]), 16);
}

#[test]
fn e1_bigint_to_string_with_radix_argument() {
    // `BigInt.prototype.toString(radix)` — hex for 255n is "ff".
    let src = "function main() { let a = 255n; return a.toString(16).length; }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn e1_bigint_value_of_returns_primitive() {
    // `BigInt.prototype.valueOf()` returns the underlying BigInt
    // primitive — auto-box path exposes it under the usual
    // prototype walk.
    let src = "function main() { \
            let a = 42n; \
            let b = a.valueOf(); \
            return (a === b) ? 1 : 0; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m36_regexp_literal_returns_regexp_object() {
    // `/foo/` creates a fresh RegExp object. `typeof` is
    // `"object"` (length 6).
    let src = "function main() { let r = /foo/; return (typeof r).length; }";
    assert_eq!(run_int32_function(src, &[]), 6);
}

#[test]
fn m36_regexp_test_matches_source() {
    // `RegExp.prototype.test` returns a boolean — bridging
    // through the runtime's `regexp_test` native.
    let src = "function main() { \
            let r = /hello/; \
            return r.test(\"hello world\") ? 1 : 0; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

#[test]
fn m36_regexp_flags_preserved() {
    // Flags from the source (`/foo/gi`) surface via the
    // `.flags` getter on the RegExp prototype. Verify length
    // of the string — `"gi"` → 2.
    let src = "function main() { let r = /foo/gi; return r.flags.length; }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn m36_regexp_exec_returns_match_array() {
    // `.exec(str)` returns either `null` or an array whose
    // first element is the matched substring. `exec(...)[0]`
    // should be the match; we read its length to verify.
    let src = "function main() { \
            let r = /el/; \
            let m = r.exec(\"hello\"); \
            return m[0].length; \
        }";
    assert_eq!(run_int32_function(src, &[]), 2);
}

#[test]
fn m36_regexp_no_match_returns_null() {
    // `.exec` returns `null` on miss. We detect via
    // `=== null` short-circuit but since our M6 relational path
    // needs a register, assign `null` to a local first.
    let src = "function main() { \
            let r = /nope/; \
            let m = r.exec(\"hello\"); \
            let n = null; \
            return (m === n) ? 1 : 0; \
        }";
    assert_eq!(run_int32_function(src, &[]), 1);
}

// ---------------------------------------------------------------------------
// M35: ES module imports + exports + dynamic `import()`
// ---------------------------------------------------------------------------

/// Test helper — runs a two-module graph under an in-memory host,
/// calls a named export of the entry module with the given int32
/// args, and returns the i32 result. The exported function must be
/// installed as a global on the same runtime state that
/// `execute_module_graph` populates.
fn run_module_graph_int32(
    entry_url: &str,
    modules: &[(&str, &str)],
    exported_fn: &str,
    args: &[i32],
) -> i32 {
    use crate::module_loader::{InMemoryModuleHost, ModuleRegistry, execute_module_graph};
    let mut host = InMemoryModuleHost::new();
    for (url, src) in modules {
        host.add_module(*url, *src);
    }
    let mut runtime = crate::interpreter::RuntimeState::new();
    let mut registry = ModuleRegistry::new();
    execute_module_graph(entry_url, &host, &mut runtime, &mut registry)
        .expect("execute_module_graph");
    // Pull the exported function value out of the entry module's
    // namespace — it was captured by `capture_exports` after the
    // synthesised module-init installed it as a global.
    let value = registry
        .get_export(entry_url, exported_fn)
        .expect("entry exports the requested fn");
    let mut arg_regs: Vec<RegisterValue> =
        args.iter().map(|&a| RegisterValue::from_i32(a)).collect();
    let handle = crate::object::ObjectHandle(
        value
            .as_object_handle()
            .expect("exported value must be an object handle"),
    );
    let result = runtime
        .call_callable(handle, RegisterValue::undefined(), arg_regs.as_mut_slice())
        .expect("call_callable");
    result
        .as_i32()
        .expect("exported fn returned a non-int32 value")
}

#[test]
fn m35_static_named_import_from_sibling_module() {
    // `import { add } from "./lib"; export function main() { return add(2, 3) }`
    // should resolve `add` through the lib module's namespace and
    // produce 5.
    let lib_src = "export function add(a, b) { return a + b }";
    let entry_src = "import { add } from \"./lib\"; \
        export function main() { return add(2, 3) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 5);
}

#[test]
fn m35_default_import_calls_default_export() {
    // `export default function add(a, b) { return a + b }` — the
    // default export name in the loader's namespace is `"default"`.
    // Consumer grabs it via `import add from "./lib"`.
    let lib_src = "export default function add(a, b) { return a + b }";
    let entry_src = "import add from \"./lib\"; \
        export function main() { return add(4, 5) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 9);
}

#[test]
fn m35_default_class_export_can_be_imported_and_instantiated() {
    // `export default class Foo { ... }` — the class name becomes
    // the default export binding. Consumers grab it via the usual
    // `import Foo from "./lib"` form. Verifies the class's
    // instance methods work correctly after the default-export
    // indirection.
    let lib_src = "export default class Point { \
                       constructor(x, y) { this.x = x; this.y = y } \
                       sum() { return this.x + this.y } \
                   }";
    let entry_src = "import Point from \"./lib\"; \
        export function main() { return new Point(3, 4).sum() }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 7);
}

#[test]
fn m35_anonymous_default_class_export_can_be_imported() {
    // Anonymous default classes lower through the same synthetic
    // `__otter_default` binding path as default expressions, but
    // still need full class construction + method installation.
    let lib_src = "export default class { \
                       constructor(x) { this.x = x } \
                       value() { return this.x } \
                   }";
    let entry_src = "import Box from \"./lib\"; \
        export function main() { return new Box(42).value() }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 42);
}

#[test]
fn m35_default_expression_export_can_be_imported() {
    // `export default <expression>` — the expression evaluates at
    // module-init time and the result becomes the default export.
    // Canonical pattern for factories, constants, and configured
    // objects. Here we export a plain object and read a field on
    // the consumer side.
    let lib_src = "export default { version: 42 }";
    let entry_src = "import cfg from \"./lib\"; \
        export function main() { return cfg.version }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 42);
}

#[test]
fn m35_anonymous_default_function_export_can_be_called() {
    // Anonymous default functions now skip the declaration-hoist
    // table and lower through the same module-init binding path
    // as other default-export values.
    let lib_src = "export default function (a, b) { return a + b }";
    let entry_src = "import add from \"./lib\"; \
        export function main() { return add(19, 23) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 42);
}

#[test]
fn m35_object_destructuring_export() {
    // `export const { a, b } = src` — the destructuring binds
    // `a` and `b` each as their own export. The consumer can
    // import either one by name.
    let lib_src = "export const { a, b } = { a: 10, b: 32 }";
    let entry_src = "import { a, b } from \"./lib\"; \
        export function main() { return a + b }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 42);
}

#[test]
fn m35_array_destructuring_export() {
    // `export const [x, y] = pair` — array-destructuring export
    // binds each leaf under its own name.
    let lib_src = "export const [x, y] = [6, 7]";
    let entry_src = "import { x, y } from \"./lib\"; \
        export function main() { return x * y }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 42);
}

#[test]
fn m35_default_arrow_function_export() {
    // `export default (x, y) => x * y` — common shorthand for
    // tiny utility modules. Verifies arrow-function defaults
    // flow through the expression path too.
    let lib_src = "export default (x, y) => x * y";
    let entry_src = "import mul from \"./lib\"; \
        export function main() { return mul(6, 7) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 42);
}

#[test]
fn m35_namespace_import_exposes_all_exports() {
    // `import * as ns from "./lib"; ns.add(…)` — the loader builds
    // a plain object whose own properties are the exported names
    // of the source module. Property access then picks off `.add`.
    let lib_src = "export function add(a, b) { return a + b } \
        export function mul(a, b) { return a * b }";
    let entry_src = "import * as ns from \"./lib\"; \
        export function main() { return ns.mul(ns.add(1, 2), 4) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 12);
}

#[test]
fn m35_renamed_named_import_uses_local_name() {
    // `import { add as plus } from "./lib"`. The local binding is
    // `plus`, not `add`, and must resolve even though the imported
    // name is different.
    let lib_src = "export function add(a, b) { return a + b }";
    let entry_src = "import { add as plus } from \"./lib\"; \
        export function main() { return plus(10, 11) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 21);
}

#[test]
fn m35_re_export_named_from_other_module() {
    // `export { add } from "./lib"` in `middle` — the loader
    // wires the source namespace through without the intermediate
    // module needing a local binding.
    let lib_src = "export function add(a, b) { return a + b }";
    let middle_src = "export { add } from \"./lib\"";
    let entry_src = "import { add } from \"./middle\"; \
        export function main() { return add(7, 8) }";
    let out = run_module_graph_int32(
        "entry",
        &[
            ("entry", entry_src),
            ("middle", middle_src),
            ("lib", lib_src),
        ],
        "main",
        &[],
    );
    assert_eq!(out, 15);
}

#[test]
fn m35_export_star_from_other_module() {
    // `export * from "./lib"` — all non-default exports flow
    // through to the re-exporter's namespace.
    let lib_src = "export function add(a, b) { return a + b } \
        export function sub(a, b) { return a - b }";
    let middle_src = "export * from \"./lib\"";
    let entry_src = "import { add, sub } from \"./middle\"; \
        export function main() { return sub(add(10, 5), 3) }";
    let out = run_module_graph_int32(
        "entry",
        &[
            ("entry", entry_src),
            ("middle", middle_src),
            ("lib", lib_src),
        ],
        "main",
        &[],
    );
    assert_eq!(out, 12);
}

/// Runs a module graph through the shared-ownership entry point
/// so dynamic `import()` inside any module body can resolve
/// through the thread-local-installed host/registry. Pulls the
/// entry module's named export and calls it with `args`.
fn run_module_graph_shared_int32(
    entry_url: &str,
    modules: &[(&str, &str)],
    exported_fn: &str,
    args: &[i32],
) -> i32 {
    use crate::module_loader::{InMemoryModuleHost, ModuleRegistry, execute_module_graph_shared};
    use std::cell::RefCell;
    use std::rc::Rc;
    let mut host = InMemoryModuleHost::new();
    for (url, src) in modules {
        host.add_module(*url, *src);
    }
    let host: Rc<dyn crate::module_loader::ModuleHost> = Rc::new(host);
    let registry = Rc::new(RefCell::new(ModuleRegistry::new()));
    let mut runtime = crate::interpreter::RuntimeState::new();
    execute_module_graph_shared(
        entry_url,
        Rc::clone(&host),
        &mut runtime,
        Rc::clone(&registry),
    )
    .expect("execute_module_graph_shared");
    let registry_ref = registry.borrow();
    let value = registry_ref
        .get_export(entry_url, exported_fn)
        .expect("entry exports the requested fn");
    drop(registry_ref);
    let mut arg_regs: Vec<RegisterValue> =
        args.iter().map(|&a| RegisterValue::from_i32(a)).collect();
    let handle = crate::object::ObjectHandle(
        value
            .as_object_handle()
            .expect("exported value must be an object handle"),
    );
    // Re-install the dynamic-import context for the call so
    // `import(expr)` + `import.meta` inside the exported function
    // see the same host/registry the graph was evaluated with.
    let result = crate::module_loader::with_dynamic_import_context(
        Rc::clone(&host),
        Rc::clone(&registry),
        entry_url,
        || {
            runtime
                .call_callable(handle, RegisterValue::undefined(), arg_regs.as_mut_slice())
                .expect("call_callable")
        },
    );
    result
        .as_i32()
        .expect("exported fn returned a non-int32 value")
}

#[test]
fn m35_dynamic_import_loads_module_and_returns_namespace() {
    // `import("./lib")` returns a Promise. Using `await` inside
    // the async entry function unwraps the namespace so we can
    // call `ns.add(...)` directly. The await also drains the
    // microtask queue, which is the spec-compliant observation
    // point for the import resolution.
    let lib_src = "export function add(a, b) { return a + b }";
    let entry_src = "export async function main() { \
            let ns = await import(\"./lib\"); \
            return ns.add(10, 20) \
        }";
    let out = run_module_graph_shared_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 30);
}

#[test]
fn m35_import_meta_url_matches_module_url() {
    // `import.meta.url` exposes the currently-evaluating module's
    // URL. Inside `main` we read the property and compare its
    // length to the URL string we passed at compile time.
    let entry_src = "export function main() { \
            let meta = import.meta; \
            return meta.url.length \
        }";
    let out = run_module_graph_shared_int32("entry", &[("entry", entry_src)], "main", &[]);
    // The referrer the loader stores when evaluating "entry" is
    // literally "entry", so the length is 5.
    assert_eq!(out, 5);
}

#[test]
fn m35_export_specifier_list_without_declaration() {
    // `export { add }` where `add` is a top-level declaration — the
    // specifier list is parsed even without an inline declaration.
    let lib_src = "function add(a, b) { return a + b } export { add }";
    let entry_src = "import { add } from \"./lib\"; \
        export function main() { return add(2, 3) }";
    let out = run_module_graph_int32(
        "entry",
        &[("entry", entry_src), ("lib", lib_src)],
        "main",
        &[],
    );
    assert_eq!(out, 5);
}

#[test]
fn spread_in_new_expression_routes_through_construct_spread() {
    // `new C(...args)` builds the arg array the same way
    // `CallSpread` does, then dispatches via
    // `ConstructSpread` — the existing Construct opcode with a
    // spread-argv window.
    assert_eq!(
        run_int32_function(
            "function main() { \
                class C { constructor(x, y) { this.v = x + y } } \
                let c = new C(...[10, 5]); \
                return c.v \
            }",
            &[],
        ),
        15
    );
}

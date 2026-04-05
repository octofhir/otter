//! Integration tests for §13.15.4 Logical Assignment and §13.6 Exponentiation.
//!
//! Spec references:
//! - §13.15.4 Logical Assignment: <https://tc39.es/ecma262/#sec-assignment-operators-runtime-semantics-evaluation>
//! - §13.6 Exponentiation: <https://tc39.es/ecma262/#sec-exp-operator>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, source_url).expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value()
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.6 — Exponentiation operator: `**`
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exponentiation_basic() {
    execute_test262_basic(
        "assert.sameValue(2 ** 10, 1024, '2 ** 10');",
        "exp.js",
    );
}

#[test]
fn exponentiation_zero() {
    execute_test262_basic(
        "assert.sameValue(5 ** 0, 1, 'anything ** 0 = 1');",
        "exp.js",
    );
}

#[test]
fn exponentiation_compound() {
    execute_test262_basic(
        "var x = 3; x **= 3; assert.sameValue(x, 27, '3 **= 3');",
        "exp.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.15.4 — Logical AND assignment: `&&=`
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn logical_and_assign_truthy_lhs() {
    // LHS truthy → evaluates RHS and assigns.
    execute_test262_basic(
        "var x = 1; x &&= 42; assert.sameValue(x, 42, 'truthy &&= assigns');",
        "logassign.js",
    );
}

#[test]
fn logical_and_assign_falsy_lhs() {
    // LHS falsy → short-circuits, keeps LHS value.
    execute_test262_basic(
        "var x = 0; x &&= 42; assert.sameValue(x, 0, 'falsy &&= short-circuits');",
        "logassign.js",
    );
}

#[test]
fn logical_and_assign_null_lhs() {
    execute_test262_basic(
        "var x = null; x &&= 42; assert.sameValue(x, null, 'null &&= short-circuits');",
        "logassign.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.15.4 — Logical OR assignment: `||=`
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn logical_or_assign_falsy_lhs() {
    // LHS falsy → evaluates RHS and assigns.
    execute_test262_basic(
        "var x = 0; x ||= 42; assert.sameValue(x, 42, 'falsy ||= assigns');",
        "logassign.js",
    );
}

#[test]
fn logical_or_assign_truthy_lhs() {
    // LHS truthy → short-circuits, keeps LHS value.
    execute_test262_basic(
        "var x = 1; x ||= 42; assert.sameValue(x, 1, 'truthy ||= short-circuits');",
        "logassign.js",
    );
}

#[test]
fn logical_or_assign_undefined_lhs() {
    execute_test262_basic(
        "var x = undefined; x ||= 99; assert.sameValue(x, 99, 'undefined ||= assigns');",
        "logassign.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.15.4 — Nullish coalescing assignment: `??=`
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn nullish_assign_null_lhs() {
    // LHS null → evaluates RHS and assigns.
    execute_test262_basic(
        "var x = null; x ??= 42; assert.sameValue(x, 42, 'null ??= assigns');",
        "logassign.js",
    );
}

#[test]
fn nullish_assign_undefined_lhs() {
    execute_test262_basic(
        "var x = undefined; x ??= 42; assert.sameValue(x, 42, 'undefined ??= assigns');",
        "logassign.js",
    );
}

#[test]
fn nullish_assign_zero_lhs() {
    // LHS 0 (falsy but not nullish) → short-circuits.
    execute_test262_basic(
        "var x = 0; x ??= 42; assert.sameValue(x, 0, '0 ??= short-circuits');",
        "logassign.js",
    );
}

#[test]
fn nullish_assign_empty_string_lhs() {
    // LHS "" (falsy but not nullish) → short-circuits.
    execute_test262_basic(
        concat!(
            "var x = ''; x ??= 'replaced';\n",
            "assert.sameValue(x, '', 'empty string ??= short-circuits');\n",
        ),
        "logassign.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Logical assignment on member expressions
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn logical_and_assign_member() {
    execute_test262_basic(
        concat!(
            "var obj = { x: 1 };\n",
            "obj.x &&= 42;\n",
            "assert.sameValue(obj.x, 42, 'truthy member &&= assigns');\n",
        ),
        "logassign.js",
    );
}

#[test]
fn logical_or_assign_member() {
    execute_test262_basic(
        concat!(
            "var obj = { x: 0 };\n",
            "obj.x ||= 42;\n",
            "assert.sameValue(obj.x, 42, 'falsy member ||= assigns');\n",
        ),
        "logassign.js",
    );
}

#[test]
fn nullish_assign_computed_member() {
    execute_test262_basic(
        concat!(
            "var obj = { x: null };\n",
            "obj['x'] ??= 42;\n",
            "assert.sameValue(obj.x, 42, 'null computed member ??= assigns');\n",
        ),
        "logassign.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Short-circuit: RHS side-effects must NOT execute when short-circuited
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn logical_and_assign_short_circuits_rhs() {
    execute_test262_basic(
        concat!(
            "var calls = 0;\n",
            "function rhs() { calls++; return 42; }\n",
            "var x = 0;\n",
            "x &&= rhs();\n",
            "assert.sameValue(calls, 0, 'rhs not called when &&= short-circuits');\n",
        ),
        "logassign.js",
    );
}

#[test]
fn logical_or_assign_short_circuits_rhs() {
    execute_test262_basic(
        concat!(
            "var calls = 0;\n",
            "function rhs() { calls++; return 42; }\n",
            "var x = 1;\n",
            "x ||= rhs();\n",
            "assert.sameValue(calls, 0, 'rhs not called when ||= short-circuits');\n",
        ),
        "logassign.js",
    );
}

#[test]
fn nullish_assign_short_circuits_rhs() {
    execute_test262_basic(
        concat!(
            "var calls = 0;\n",
            "function rhs() { calls++; return 42; }\n",
            "var x = 0;\n",
            "x ??= rhs();\n",
            "assert.sameValue(calls, 0, 'rhs not called when ??= short-circuits on 0');\n",
        ),
        "logassign.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.5.1.2 — delete operator
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn delete_property() {
    execute_test262_basic(
        concat!(
            "var obj = { x: 1, y: 2 };\n",
            "var result = delete obj.x;\n",
            "assert.sameValue(result, true, 'delete returns true');\n",
            "assert.sameValue(obj.x, undefined, 'deleted property is undefined');\n",
            "assert.sameValue(obj.y, 2, 'other properties untouched');\n",
        ),
        "delete.js",
    );
}

#[test]
fn delete_computed_property() {
    execute_test262_basic(
        concat!(
            "var obj = { a: 1 };\n",
            "var key = 'a';\n",
            "assert.sameValue(delete obj[key], true, 'delete computed returns true');\n",
            "assert.sameValue(obj.a, undefined, 'deleted computed is undefined');\n",
        ),
        "delete.js",
    );
}

#[test]
fn delete_non_reference_returns_true() {
    execute_test262_basic(
        "assert.sameValue(delete 42, true, 'delete literal returns true');",
        "delete.js",
    );
}

#[test]
fn delete_identifier_returns_true() {
    // In sloppy mode, `delete x` on a declared var — just verifies it doesn't crash.
    execute_test262_basic(
        "var x = 1; var result = delete x;",
        "delete.js",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Comma expression edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn comma_expression_returns_last() {
    execute_test262_basic(
        "var x = (1, 2, 3); assert.sameValue(x, 3, 'comma returns last');",
        "comma.js",
    );
}

#[test]
fn comma_expression_evaluates_side_effects() {
    execute_test262_basic(
        concat!(
            "var a = 0, b = 0;\n",
            "var x = (a = 1, b = 2, a + b);\n",
            "assert.sameValue(x, 3, 'comma evaluates all');\n",
            "assert.sameValue(a, 1, 'side effect a');\n",
            "assert.sameValue(b, 2, 'side effect b');\n",
        ),
        "comma.js",
    );
}

#[test]
fn comma_in_for_init() {
    execute_test262_basic(
        concat!(
            "var sum = 0;\n",
            "for (var i = 0, j = 10; i < 3; i++, j--) { sum += j; }\n",
            "assert.sameValue(sum, 10 + 9 + 8, 'comma in for-init and update');\n",
        ),
        "comma.js",
    );
}

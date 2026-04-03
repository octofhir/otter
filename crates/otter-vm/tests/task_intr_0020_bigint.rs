//! Integration tests for ES2024 BigInt (§21.2).
//!
//! Spec references:
//! - BigInt Objects: <https://tc39.es/ecma262/#sec-bigint-objects>
//! - BigInt Constructor: <https://tc39.es/ecma262/#sec-bigint-constructor-number-value>
//! - BigInt.asIntN: <https://tc39.es/ecma262/#sec-bigint.asintn>
//! - BigInt.asUintN: <https://tc39.es/ecma262/#sec-bigint.asuintn>
//! - BigInt.prototype.toString: <https://tc39.es/ecma262/#sec-bigint.prototype.tostring>
//! - BigInt.prototype.valueOf: <https://tc39.es/ecma262/#sec-bigint.prototype.valueof>

use otter_vm::source::compile_eval;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str) -> RegisterValue {
    let module = compile_eval(source, "<test>").expect("should compile");
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

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
}

fn run_string(source: &str, runtime: &mut RuntimeState) -> String {
    let module = compile_eval(source, "<test>").expect("should compile");
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    let v = Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            runtime,
        )
        .expect("should execute")
        .return_value();
    let handle = v
        .as_object_handle()
        .unwrap_or_else(|| panic!("expected string, got {v:?}"));
    runtime
        .objects()
        .string_value(otter_vm::object::ObjectHandle(handle))
        .unwrap()
        .unwrap()
        .to_string()
}

// ── Literal compilation ─────────────────────────────────────────────

#[test]
fn bigint_literal_zero() {
    assert!(run("0n").is_bigint());
}

#[test]
fn bigint_literal_positive() {
    assert!(run("42n").is_bigint());
}

#[test]
fn bigint_literal_large() {
    // Arbitrary-precision — should not overflow.
    assert!(run("123456789012345678901234567890n").is_bigint());
}

#[test]
fn bigint_literal_negative() {
    assert!(run("-1n").is_bigint());
}

// ── typeof ──────────────────────────────────────────────────────────

#[test]
fn typeof_bigint() {
    assert!(run_bool("typeof 1n === 'bigint'"));
}

#[test]
fn typeof_bigint_zero() {
    assert!(run_bool("typeof 0n === 'bigint'"));
}

// ── Arithmetic ──────────────────────────────────────────────────────

#[test]
fn bigint_add() {
    assert!(run_bool("1n + 2n === 3n"));
}

#[test]
fn bigint_sub() {
    assert!(run_bool("10n - 3n === 7n"));
}

#[test]
fn bigint_mul() {
    assert!(run_bool("6n * 7n === 42n"));
}

#[test]
fn bigint_div() {
    // BigInt division truncates toward zero.
    assert!(run_bool("7n / 2n === 3n"));
}

#[test]
fn bigint_mod() {
    assert!(run_bool("7n % 3n === 1n"));
}

#[test]
fn bigint_unary_minus() {
    assert!(run_bool("-42n === -42n"));
}

#[test]
fn bigint_large_arithmetic() {
    assert!(run_bool(
        "123456789012345678901234567890n + 10n === 123456789012345678901234567900n"
    ));
}

// ── Mixed BigInt + Number → TypeError ──────────────────────────────

#[test]
fn bigint_add_mixed_throws() {
    assert!(run_bool(
        "try { 1n + 1; false; } catch (e) { e instanceof TypeError; }"
    ));
}

#[test]
fn bigint_sub_mixed_throws() {
    assert!(run_bool(
        "try { 1n - 1; false; } catch (e) { e instanceof TypeError; }"
    ));
}

// ── Comparison ──────────────────────────────────────────────────────

#[test]
fn bigint_strict_eq() {
    assert!(run_bool("1n === 1n"));
}

#[test]
fn bigint_strict_eq_different_values() {
    assert!(run_bool("1n !== 2n"));
}

#[test]
fn bigint_strict_neq_number() {
    // BigInt and Number are different types → strict inequality.
    assert!(run_bool("1n !== 1"));
}

#[test]
fn bigint_loose_eq_number() {
    // §7.2.15 IsLooselyEqual: BigInt == Number compares mathematically.
    assert!(run_bool("1n == 1"));
}

#[test]
fn bigint_loose_eq_number_false() {
    assert!(run_bool("!(1n == 2)"));
}

#[test]
fn bigint_less_than() {
    assert!(run_bool("1n < 2n"));
}

#[test]
fn bigint_greater_than() {
    assert!(run_bool("2n > 1n"));
}

#[test]
fn bigint_less_than_or_equal() {
    assert!(run_bool("1n <= 1n"));
}

#[test]
fn bigint_greater_than_or_equal() {
    assert!(run_bool("2n >= 1n"));
}

// ── Boolean coercion ────────────────────────────────────────────────

#[test]
fn bigint_zero_is_falsy() {
    assert!(run_bool("!0n"));
}

#[test]
fn bigint_nonzero_is_truthy() {
    assert!(run_bool("!!1n"));
}

// ── Constructor ─────────────────────────────────────────────────────

#[test]
fn bigint_constructor_from_int() {
    assert!(run_bool("BigInt(42) === 42n"));
}

#[test]
fn bigint_constructor_from_bool_true() {
    assert!(run_bool("BigInt(true) === 1n"));
}

#[test]
fn bigint_constructor_from_bool_false() {
    assert!(run_bool("BigInt(false) === 0n"));
}

#[test]
fn bigint_constructor_from_string() {
    assert!(run_bool("BigInt('42') === 42n"));
}

#[test]
fn bigint_constructor_not_constructable() {
    assert!(run_bool(
        "try { new BigInt(1); false; } catch (e) { e instanceof TypeError; }"
    ));
}

// ── Prototype methods ───────────────────────────────────────────────

#[test]
fn bigint_to_string_decimal() {
    let mut runtime = RuntimeState::new();
    let s = run_string("(42n).toString()", &mut runtime);
    assert_eq!(s, "42");
}

#[test]
fn bigint_to_string_hex() {
    let mut runtime = RuntimeState::new();
    let s = run_string("(255n).toString(16)", &mut runtime);
    assert_eq!(s, "ff");
}

#[test]
fn bigint_to_string_binary() {
    let mut runtime = RuntimeState::new();
    let s = run_string("(10n).toString(2)", &mut runtime);
    assert_eq!(s, "1010");
}

#[test]
fn bigint_value_of() {
    assert!(run_bool("(42n).valueOf() === 42n"));
}

// ── Static methods ──────────────────────────────────────────────────

#[test]
fn bigint_as_int_n() {
    // BigInt.asIntN(8, 255n) === -1n (signed 8-bit wrapping)
    assert!(run_bool("BigInt.asIntN(8, 255n) === -1n"));
}

#[test]
fn bigint_as_uint_n() {
    // BigInt.asUintN(8, 256n) === 0n (unsigned 8-bit wrapping)
    assert!(run_bool("BigInt.asUintN(8, 256n) === 0n"));
}

#[test]
fn bigint_as_uint_n_positive() {
    assert!(run_bool("BigInt.asUintN(8, 255n) === 255n"));
}

// ── toString coercion ───────────────────────────────────────────────

#[test]
fn bigint_string_concatenation() {
    let mut runtime = RuntimeState::new();
    let s = run_string("'' + 42n", &mut runtime);
    assert_eq!(s, "42");
}

#[test]
fn bigint_template_literal() {
    let mut runtime = RuntimeState::new();
    let s = run_string("`value: ${42n}`", &mut runtime);
    assert_eq!(s, "value: 42");
}

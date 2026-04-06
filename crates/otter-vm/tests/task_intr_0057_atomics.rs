//! Integration tests for Step 62 — Atomics namespace (§25.4)
//!
//! Phase 1: Single-threaded correct implementations.
//! All atomic operations are trivially atomic in a single-threaded VM.
//!
//! - §25.4.1 Atomics.add: <https://tc39.es/ecma262/#sec-atomics.add>
//! - §25.4.2 Atomics.and: <https://tc39.es/ecma262/#sec-atomics.and>
//! - §25.4.4 Atomics.compareExchange: <https://tc39.es/ecma262/#sec-atomics.compareexchange>
//! - §25.4.5 Atomics.exchange: <https://tc39.es/ecma262/#sec-atomics.exchange>
//! - §25.4.6 Atomics.isLockFree: <https://tc39.es/ecma262/#sec-atomics.islockfree>
//! - §25.4.7 Atomics.load: <https://tc39.es/ecma262/#sec-atomics.load>
//! - §25.4.8 Atomics.notify: <https://tc39.es/ecma262/#sec-atomics.notify>
//! - §25.4.9 Atomics.or: <https://tc39.es/ecma262/#sec-atomics.or>
//! - §25.4.11 Atomics.store: <https://tc39.es/ecma262/#sec-atomics.store>
//! - §25.4.12 Atomics.sub: <https://tc39.es/ecma262/#sec-atomics.sub>
//! - §25.4.13 Atomics.wait: <https://tc39.es/ecma262/#sec-atomics.wait>
//! - §25.4.14 Atomics.xor: <https://tc39.es/ecma262/#sec-atomics.xor>

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

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
}

fn run_f64(source: &str) -> f64 {
    let v = run(source);
    v.as_number()
        .unwrap_or_else(|| panic!("expected number, got {v:?}"))
}

fn run_string(source: &str) -> String {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    let v = Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value();
    let handle = v.as_object_handle().expect("expected string handle");
    runtime
        .objects()
        .string_value(otter_vm::object::ObjectHandle(handle))
        .expect("string lookup")
        .expect("string value")
        .to_string()
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics namespace existence & @@toStringTag
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn atomics_exists() {
    assert!(run_bool("typeof Atomics === 'object'"));
}

#[test]
fn atomics_to_string_tag() {
    assert_eq!(
        run_string("Object.prototype.toString.call(Atomics)"),
        "[object Atomics]"
    );
}

#[test]
fn atomics_not_a_function() {
    assert!(run_bool("typeof Atomics !== 'function'"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.isLockFree
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn is_lock_free_1() {
    assert!(run_bool("Atomics.isLockFree(1)"));
}

#[test]
fn is_lock_free_2() {
    assert!(run_bool("Atomics.isLockFree(2)"));
}

#[test]
fn is_lock_free_4() {
    assert!(run_bool("Atomics.isLockFree(4)"));
}

#[test]
fn is_lock_free_8() {
    assert!(run_bool("Atomics.isLockFree(8)"));
}

#[test]
fn is_lock_free_3_false() {
    assert!(!run_bool("Atomics.isLockFree(3)"));
}

#[test]
fn is_lock_free_6_false() {
    assert!(!run_bool("Atomics.isLockFree(6)"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.load / Atomics.store — Int32Array
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_store_int32() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            Atomics.store(ta, 0, 42);
            Atomics.load(ta, 0);
            "#
        ),
        42
    );
}

#[test]
fn store_returns_coerced_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            Atomics.store(ta, 0, 123);
            "#
        ),
        123
    );
}

#[test]
fn load_default_zero() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            Atomics.load(ta, 0);
            "#
        ),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.load / Atomics.store — Int8Array
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_store_int8() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(8);
            var ta = new Int8Array(sab);
            Atomics.store(ta, 0, -5);
            Atomics.load(ta, 0);
            "#
        ),
        -5
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.load / Atomics.store — Uint8Array
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_store_uint8() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(8);
            var ta = new Uint8Array(sab);
            Atomics.store(ta, 0, 255);
            Atomics.load(ta, 0);
            "#
        ),
        255
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.load / Atomics.store — Int16Array
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_store_int16() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int16Array(sab);
            Atomics.store(ta, 1, -1000);
            Atomics.load(ta, 1);
            "#
        ),
        -1000
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.load / Atomics.store — Uint32Array
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_store_uint32() {
    // Uint32 values > 2^31 come back as f64
    let v = run_f64(
        r#"
        var sab = new SharedArrayBuffer(16);
        var ta = new Uint32Array(sab);
        Atomics.store(ta, 0, 3000000000);
        Atomics.load(ta, 0);
        "#,
    );
    assert_eq!(v, 3_000_000_000.0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.add
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn add_returns_old_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 10;
            Atomics.add(ta, 0, 5);
            "#
        ),
        10 // returns old value
    );
}

#[test]
fn add_writes_new_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 10;
            Atomics.add(ta, 0, 5);
            Atomics.load(ta, 0);
            "#
        ),
        15
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.sub
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sub_returns_old_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 20;
            Atomics.sub(ta, 0, 7);
            "#
        ),
        20
    );
}

#[test]
fn sub_writes_new_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 20;
            Atomics.sub(ta, 0, 7);
            Atomics.load(ta, 0);
            "#
        ),
        13
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.and
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn and_operation() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 0xFF;
            Atomics.and(ta, 0, 0x0F);
            Atomics.load(ta, 0);
            "#
        ),
        0x0F
    );
}

#[test]
fn and_returns_old_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 0xFF;
            Atomics.and(ta, 0, 0x0F);
            "#
        ),
        0xFF
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.or
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn or_operation() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 0xF0;
            Atomics.or(ta, 0, 0x0F);
            Atomics.load(ta, 0);
            "#
        ),
        0xFF
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.xor
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xor_operation() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 0xFF;
            Atomics.xor(ta, 0, 0x0F);
            Atomics.load(ta, 0);
            "#
        ),
        0xF0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.exchange
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exchange_returns_old_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 99;
            Atomics.exchange(ta, 0, 42);
            "#
        ),
        99
    );
}

#[test]
fn exchange_writes_new_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 99;
            Atomics.exchange(ta, 0, 42);
            Atomics.load(ta, 0);
            "#
        ),
        42
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.compareExchange
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn compare_exchange_match() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 5;
            Atomics.compareExchange(ta, 0, 5, 10);
            Atomics.load(ta, 0);
            "#
        ),
        10 // matched, so replacement written
    );
}

#[test]
fn compare_exchange_no_match() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 5;
            Atomics.compareExchange(ta, 0, 99, 10);
            Atomics.load(ta, 0);
            "#
        ),
        5 // no match, original value retained
    );
}

#[test]
fn compare_exchange_returns_old_value() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 5;
            Atomics.compareExchange(ta, 0, 5, 10);
            "#
        ),
        5 // always returns old value regardless of match
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.wait — single-threaded
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn wait_not_equal() {
    assert_eq!(
        run_string(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 10;
            Atomics.wait(ta, 0, 0);
            "#
        ),
        "not-equal"
    );
}

#[test]
fn wait_timed_out() {
    assert_eq!(
        run_string(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 0;
            Atomics.wait(ta, 0, 0, 0);
            "#
        ),
        "timed-out"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.notify — single-threaded
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn notify_returns_zero() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            Atomics.notify(ta, 0, 1);
            "#
        ),
        0
    );
}

#[test]
fn notify_undefined_count_returns_zero() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            Atomics.notify(ta, 0);
            "#
        ),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Error handling — type validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_rejects_float64_array() {
    assert!(run_bool(
        r#"
        var sab = new SharedArrayBuffer(16);
        var ta = new Float64Array(sab);
        try { Atomics.load(ta, 0); false; } catch(e) { e instanceof TypeError; }
        "#
    ));
}

#[test]
fn load_rejects_non_typed_array() {
    assert!(run_bool(
        r#"
        try { Atomics.load({}, 0); false; } catch(e) { e instanceof TypeError; }
        "#
    ));
}

#[test]
fn wait_rejects_uint32_array() {
    // wait only accepts Int32Array and BigInt64Array
    assert!(run_bool(
        r#"
        var sab = new SharedArrayBuffer(16);
        var ta = new Uint32Array(sab);
        try { Atomics.wait(ta, 0, 0); false; } catch(e) { e instanceof TypeError; }
        "#
    ));
}

#[test]
fn load_out_of_range_throws_range_error() {
    assert!(run_bool(
        r#"
        var sab = new SharedArrayBuffer(16);
        var ta = new Int32Array(sab);
        try { Atomics.load(ta, 100); false; } catch(e) { e instanceof RangeError; }
        "#
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Multiple operations sequence
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sequential_operations() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            Atomics.store(ta, 0, 100);
            Atomics.add(ta, 0, 50);      // 100 → 150
            Atomics.sub(ta, 0, 30);      // 150 → 120
            Atomics.and(ta, 0, 0x7F);    // 120 & 127 = 120
            Atomics.or(ta, 0, 0x100);    // 120 | 256 = 376
            Atomics.load(ta, 0);
            "#
        ),
        376
    );
}

#[test]
fn multiple_indices() {
    assert!(run_bool(
        r#"
        var sab = new SharedArrayBuffer(16);
        var ta = new Int32Array(sab);
        Atomics.store(ta, 0, 1);
        Atomics.store(ta, 1, 2);
        Atomics.store(ta, 2, 3);
        Atomics.store(ta, 3, 4);
        Atomics.load(ta, 0) === 1 &&
        Atomics.load(ta, 1) === 2 &&
        Atomics.load(ta, 2) === 3 &&
        Atomics.load(ta, 3) === 4;
        "#
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Uint16Array operations
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn load_store_uint16() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Uint16Array(sab);
            Atomics.store(ta, 0, 65535);
            Atomics.load(ta, 0);
            "#
        ),
        65535
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Int8Array wrapping arithmetic
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn add_int8_wraps() {
    assert_eq!(
        run_i32(
            r#"
            var sab = new SharedArrayBuffer(8);
            var ta = new Int8Array(sab);
            Atomics.store(ta, 0, 127);
            Atomics.add(ta, 0, 1);
            Atomics.load(ta, 0);
            "#
        ),
        -128 // wraps around
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Atomics.wait with matching value and infinite timeout
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn wait_matching_value_returns_timed_out() {
    // In single-threaded mode, even with matching value, we return "timed-out"
    // because there's no other thread to notify us.
    assert_eq!(
        run_string(
            r#"
            var sab = new SharedArrayBuffer(16);
            var ta = new Int32Array(sab);
            ta[0] = 0;
            Atomics.wait(ta, 0, 0, 0);
            "#
        ),
        "timed-out"
    );
}

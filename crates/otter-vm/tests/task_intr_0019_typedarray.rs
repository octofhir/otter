//! Integration tests for ES2024 TypedArray (§23.2).
//!
//! Spec references:
//! - TypedArray constructors: <https://tc39.es/ecma262/#sec-typedarray-constructors>
//! - %TypedArray%.prototype methods: <https://tc39.es/ecma262/#sec-%typedarray%.prototype>
//! - %TypedArray%.from / .of: <https://tc39.es/ecma262/#sec-%typedarray%.from>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str, url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, url).expect("should compile");
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

// ── §23.2.5 Constructor forms ────────────────────────────────────────

#[test]
fn typed_array_constructor_length() {
    let r = run(
        concat!(
            "var a = new Int32Array(4);\n",
            "assert.sameValue(a.length, 4, 'length');\n",
            "assert.sameValue(a.byteLength, 16, 'byteLength = 4*4');\n",
            "assert.sameValue(a.byteOffset, 0, 'byteOffset');\n",
            "assert.sameValue(a[0], 0, 'zero-initialized');\n",
            "assert.sameValue(a[3], 0, 'zero-initialized last');\n",
        ),
        "ta-ctor-length.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_constructor_no_args() {
    let r = run(
        concat!(
            "var a = new Uint8Array();\n",
            "assert.sameValue(a.length, 0, 'empty length');\n",
            "assert.sameValue(a.byteLength, 0, 'empty byteLength');\n",
        ),
        "ta-ctor-noargs.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_constructor_from_array() {
    let r = run(
        concat!(
            "var a = new Float64Array([1.5, 2.5, 3.5]);\n",
            "assert.sameValue(a.length, 3, 'length');\n",
            "assert.sameValue(a[0], 1.5, 'elem 0');\n",
            "assert.sameValue(a[1], 2.5, 'elem 1');\n",
            "assert.sameValue(a[2], 3.5, 'elem 2');\n",
        ),
        "ta-ctor-array.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_constructor_from_typed_array() {
    let r = run(
        concat!(
            "var src = new Int16Array([10, 20, 30]);\n",
            "var dst = new Int32Array(src);\n",
            "assert.sameValue(dst.length, 3, 'length');\n",
            "assert.sameValue(dst[0], 10, 'elem 0');\n",
            "assert.sameValue(dst[1], 20, 'elem 1');\n",
            "assert.sameValue(dst[2], 30, 'elem 2');\n",
            "dst[0] = 99;\n",
            "assert.sameValue(src[0], 10, 'src unchanged');\n",
        ),
        "ta-ctor-typedarray.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_constructor_from_arraybuffer() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(12);\n",
            "var a = new Int32Array(buf, 4, 2);\n",
            "assert.sameValue(a.length, 2, 'length');\n",
            "assert.sameValue(a.byteOffset, 4, 'byteOffset');\n",
            "assert.sameValue(a.byteLength, 8, 'byteLength');\n",
            "assert.sameValue(a.buffer === buf, true, 'same buffer');\n",
        ),
        "ta-ctor-buffer.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── All 11 types construct correctly ─────────────────────────────────

#[test]
fn all_typed_array_types() {
    let r = run(
        concat!(
            "var types = [\n",
            "  ['Int8Array',         Int8Array,         1],\n",
            "  ['Uint8Array',        Uint8Array,        1],\n",
            "  ['Uint8ClampedArray', Uint8ClampedArray, 1],\n",
            "  ['Int16Array',        Int16Array,        2],\n",
            "  ['Uint16Array',       Uint16Array,       2],\n",
            "  ['Int32Array',        Int32Array,        4],\n",
            "  ['Uint32Array',       Uint32Array,       4],\n",
            "  ['Float32Array',      Float32Array,      4],\n",
            "  ['Float64Array',      Float64Array,      8],\n",
            // BigInt64Array and BigUint64Array require BigInt support
            "];\n",
            "for (var i = 0; i < types.length; i++) {\n",
            "  var name = types[i][0];\n",
            "  var Ctor = types[i][1];\n",
            "  var elemSize = types[i][2];\n",
            "  var a = new Ctor(4);\n",
            "  assert.sameValue(a.length, 4, name + '.length');\n",
            "  assert.sameValue(a.byteLength, 4 * elemSize, name + '.byteLength');\n",
            "  assert.sameValue(a.BYTES_PER_ELEMENT, elemSize, name + '.BYTES_PER_ELEMENT');\n",
            "}\n",
        ),
        "ta-all-types.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3 Prototype methods ────────────────────────────────────────

#[test]
fn typed_array_write_neg1_via_var() {
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "var x = -1;\n",
            "a[0] = x;\n",
            "assert.sameValue(a[0], -1, 'neg via var');\n",
        ),
        "ta-write-neg1-var.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_write_neg_literal_zero_minus() {
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = (0 - 1);\n",
            "assert.sameValue(a[0], -1, 'neg via 0-1');\n",
        ),
        "ta-write-neg-0minus1.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg7_via_subtraction() {
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = (0 - 7);\n",
            "assert.sameValue(a[0], -7, 'neg via 0-7');\n",
        ),
        "ta-neg7-sub.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg7_via_var() {
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "var x = 0 - 7;\n",
            "a[0] = x;\n",
            "assert.sameValue(a[0], -7, 'neg via var 0-7');\n",
        ),
        "ta-neg7-var.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg7_literal() {
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = -7;\n",
            "assert.sameValue(a[0], -7, 'neg literal -7');\n",
        ),
        "ta-neg7-literal.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_set_reads_correct() {
    // Verify write -7 reads back -7 (not 0)
    let r = run(
        "var a = new Int32Array(1); a[0] = -7; assert.sameValue(a[0], -7, 'reads -7');",
        "ta-neg-reads-correct.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_negate_vs_literal() {
    // Does regular negate work?
    let r = run(
        concat!(
            "var x = -7;\n",
            "assert.sameValue(x, -7, 'x is -7');\n",
            "var a = new Int32Array(1);\n",
            "a[0] = x;\n",
            "assert.sameValue(a[0], -7, 'a[0] is -7');\n",
        ),
        "ta-neg-negate-literal.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_inline_neg_expr() {
    // Inline expression on RHS
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = -(7);\n",
            "assert.sameValue(a[0], -7, 'a[0] is -7 from -(7)');\n",
        ),
        "ta-inline-neg-expr.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_regular_array_neg_literal() {
    // Regular array with negative literal - does this work?
    let r = run(
        concat!(
            "var a = [0];\n",
            "a[0] = -7;\n",
            "assert.sameValue(a[0], -7, 'regular array neg');\n",
        ),
        "ta-regular-neg.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_assign_return_value() {
    // Check: what does a[0] = -7 return?
    // In JS, assignment returns the RHS value.
    let r = run(
        "var a = [0]; var r = (a[0] = -7); assert.sameValue(r, -7, 'assignment returns -7');\n",
        "ta-neg-assign-rv.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_separate_lines() {
    // What if we put it on separate lines?
    let r = run(
        concat!(
            "var a = new Int32Array(1)\n",
            "var v = -7\n",
            "a[0] = v\n",
            "assert.sameValue(a[0], v, 'separate lines')\n",
        ),
        "ta-neg-sep.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_parens_expr() {
    // `a[0] = (-7)` vs `a[0] = -7`
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = (-7);\n",
            "assert.sameValue(a[0], -7, 'parens neg');\n",
        ),
        "ta-neg-parens.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_plus_zero() {
    // Does `a[0] = +0` work? What about `a[0] = +7`?
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = +7;\n",
            "assert.sameValue(a[0], 7, 'unary plus');\n",
        ),
        "ta-neg-plus.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_bitwise_or_zero() {
    // `a[0] = (-7|0)` to force integer
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = (-7|0);\n",
            "assert.sameValue(a[0], -7, 'bitwise or 0');\n",
        ),
        "ta-neg-bitor.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_mul_expr() {
    // `a[0] = 3 * 4` — multiply on RHS
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a[0] = 3 * 4;\n",
            "assert.sameValue(a[0], 12, 'mul expr');\n",
        ),
        "ta-mul-expr.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_add_expr() {
    // `a[0] = 3 + 4`
    let r = run(
        concat!(
            "var a = [0];\n",
            "a[0] = 3 + 4;\n",
            "assert.sameValue(a[0], 7, 'add expr');\n",
        ),
        "ta-add-expr.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_set_method_works() {
    // Use .set() method instead of indexed assignment
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a.set([-7]);\n",
            "assert.sameValue(a[0], -7, 'set method -7');\n",
        ),
        "ta-neg-set-method.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_construct_from_array() {
    // Construct from array with negative values
    let r = run(
        concat!(
            "var a = new Int32Array([-7, -100, 42]);\n",
            "assert.sameValue(a[0], -7, 'ctor -7');\n",
            "assert.sameValue(a[1], -100, 'ctor -100');\n",
            "assert.sameValue(a[2], 42, 'ctor 42');\n",
        ),
        "ta-neg-ctor.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_through_fill() {
    // Test negative through fill method (bypasses SetIndex fast path)
    let r = run(
        "var a = new Int32Array(1); a.fill(-7); assert.sameValue(a[0], -7, 'fill -7');",
        "ta-neg-fill.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_set_element_direct() {
    // Rust-level test: directly call typed_array_set_element via .set() with explicit number
    let r = run(
        concat!(
            "var a = new Int32Array(3);\n",
            "a[0] = 42;\n",
            "a[1] = 0;\n",
            "a[2] = 0;\n",
            "assert.sameValue(a[0], 42, 'positive set via index');\n",
            // Now test: is 42 also read correctly?
            "var v = a[0]; assert.sameValue(v, 42, 'read into var');\n",
        ),
        "ta-set-element-direct.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_neg_check_fill_then_indexed() {
    // Fill with -7, verify, then try to overwrite with indexed set
    let r = run(
        concat!(
            "var a = new Int32Array(1);\n",
            "a.fill(-7);\n",
            "assert.sameValue(a[0], -7, 'fill works');\n",
            "a[0] = -100;\n",
            "assert.sameValue(a[0], -100, 'indexed set after fill');\n",
        ),
        "ta-neg-fill-then-idx.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_indexed_access_pos() {
    let r = run(
        concat!(
            "var a = new Int32Array(3);\n",
            "a[0] = 42;\n",
            "a[2] = 100;\n",
            "assert.sameValue(a[0], 42, 'get 42');\n",
            "assert.sameValue(a[2], 100, 'get 100');\n",
        ),
        "ta-indexed-pos.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_indexed_access_oob() {
    let r = run(
        concat!(
            "var a = new Int32Array(3);\n",
            "assert.sameValue(a[3], undefined, 'out of bounds');\n",
        ),
        "ta-indexed-oob.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_uint8_clamped() {
    let r = run(
        concat!(
            "var a = new Uint8ClampedArray(3);\n",
            "a[0] = -10;\n",
            "a[1] = 300;\n",
            "a[2] = 128;\n",
            "assert.sameValue(a[0], 0, 'clamped to 0');\n",
            "assert.sameValue(a[1], 255, 'clamped to 255');\n",
            "assert.sameValue(a[2], 128, 'in range');\n",
        ),
        "ta-uint8clamped.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_int8_overflow() {
    let r = run(
        concat!(
            "var a = new Int8Array(2);\n",
            "a[0] = 200;\n",  // wraps to -56
            "a[1] = -200;\n", // wraps to 56
            "assert.sameValue(a[0], -56, '200 wraps to -56');\n",
            "assert.sameValue(a[1], 56, '-200 wraps to 56');\n",
        ),
        "ta-int8-overflow.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_uint8_overflow() {
    let r = run(
        concat!(
            "var a = new Uint8Array(2);\n",
            "a[0] = 256;\n",
            "a[1] = -1;\n",
            "assert.sameValue(a[0], 0, '256 wraps to 0');\n",
            "assert.sameValue(a[1], 255, '-1 wraps to 255');\n",
        ),
        "ta-uint8-overflow.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_float32_precision() {
    let r = run(
        concat!(
            "var a = new Float32Array(1);\n",
            "a[0] = 1.1;\n",
            // Float32 cannot represent 1.1 exactly
            "assert.sameValue(a[0] !== 1.1, true, 'float32 precision loss');\n",
            "assert.sameValue(Math.abs(a[0] - 1.1) < 0.0001, true, 'close enough');\n",
        ),
        "ta-float32.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.1 %TypedArray%.prototype.at ──────────────────────────────

#[test]
fn typed_array_at() {
    let r = run(
        concat!(
            "var a = new Int32Array([10, 20, 30]);\n",
            "assert.sameValue(a.at(0), 10, 'at(0)');\n",
            "assert.sameValue(a.at(2), 30, 'at(2)');\n",
            "assert.sameValue(a.at(-1), 30, 'at(-1)');\n",
            "assert.sameValue(a.at(-3), 10, 'at(-3)');\n",
            "assert.sameValue(a.at(3), undefined, 'at(3) OOB');\n",
            "assert.sameValue(a.at(-4), undefined, 'at(-4) OOB');\n",
        ),
        "ta-at.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.5 %TypedArray%.prototype.copyWithin ─────────────────────

#[test]
fn typed_array_copy_within() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3, 4, 5]);\n",
            "a.copyWithin(0, 3);\n",
            "assert.sameValue(a[0], 4, 'copyWithin(0,3) [0]');\n",
            "assert.sameValue(a[1], 5, 'copyWithin(0,3) [1]');\n",
            "assert.sameValue(a[2], 3, 'copyWithin(0,3) [2]');\n",
            "assert.sameValue(a[3], 4, 'copyWithin(0,3) [3]');\n",
            "assert.sameValue(a[4], 5, 'copyWithin(0,3) [4]');\n",
        ),
        "ta-copywithin.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.7 %TypedArray%.prototype.every ──────────────────────────

#[test]
fn typed_array_every() {
    let r = run(
        concat!(
            "var a = new Uint8Array([2, 4, 6]);\n",
            "var result = a.every(function(v) { return v % 2 === 0; });\n",
            "assert.sameValue(result, true, 'all even');\n",
            "var result2 = a.every(function(v) { return v > 3; });\n",
            "assert.sameValue(result2, false, 'not all > 3');\n",
        ),
        "ta-every.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.8 %TypedArray%.prototype.fill ───────────────────────────

#[test]
fn typed_array_fill() {
    let r = run(
        concat!(
            "var a = new Int32Array(5);\n",
            "a.fill(42);\n",
            "assert.sameValue(a[0], 42, 'fill all');\n",
            "assert.sameValue(a[4], 42, 'fill all end');\n",
            "a.fill(7, 2, 4);\n",
            "assert.sameValue(a[1], 42, 'fill partial [1]');\n",
            "assert.sameValue(a[2], 7, 'fill partial [2]');\n",
            "assert.sameValue(a[3], 7, 'fill partial [3]');\n",
            "assert.sameValue(a[4], 42, 'fill partial [4]');\n",
        ),
        "ta-fill.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.9 %TypedArray%.prototype.filter ─────────────────────────

#[test]
fn typed_array_filter() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3, 4, 5]);\n",
            "var b = a.filter(function(v) { return v > 3; });\n",
            "assert.sameValue(b.length, 2, 'filter length');\n",
            "assert.sameValue(b[0], 4, 'filter [0]');\n",
            "assert.sameValue(b[1], 5, 'filter [1]');\n",
            "assert.sameValue(b instanceof Int32Array, true, 'same type');\n",
        ),
        "ta-filter.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.10-13 find / findIndex / findLast / findLastIndex ───────

#[test]
fn typed_array_find_methods() {
    let r = run(
        concat!(
            "var a = new Float64Array([1.1, 2.2, 3.3, 2.2]);\n",
            "assert.sameValue(a.find(function(v) { return v > 2; }), 2.2, 'find');\n",
            "assert.sameValue(a.findIndex(function(v) { return v > 2; }), 1, 'findIndex');\n",
            "assert.sameValue(a.findLast(function(v) { return v > 2; }), 2.2, 'findLast');\n",
            "assert.sameValue(a.findLastIndex(function(v) { return v > 2; }), 3, 'findLastIndex');\n",
        ),
        "ta-find.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.14 %TypedArray%.prototype.forEach ───────────────────────

#[test]
fn typed_array_for_each() {
    let r = run(
        concat!(
            "var a = new Int32Array([10, 20, 30]);\n",
            "var sum = 0;\n",
            "a.forEach(function(v) { sum += v; });\n",
            "assert.sameValue(sum, 60, 'forEach sum');\n",
        ),
        "ta-foreach.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.15 %TypedArray%.prototype.includes ──────────────────────

#[test]
fn typed_array_includes() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3]);\n",
            "assert.sameValue(a.includes(2), true, 'includes 2');\n",
            "assert.sameValue(a.includes(4), false, 'not includes 4');\n",
            "assert.sameValue(a.includes(2, 2), false, 'includes fromIndex');\n",
        ),
        "ta-includes.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.16 %TypedArray%.prototype.indexOf ───────────────────────

#[test]
fn typed_array_index_of() {
    let r = run(
        concat!(
            "var a = new Int32Array([5, 10, 15, 10]);\n",
            "assert.sameValue(a.indexOf(10), 1, 'indexOf 10');\n",
            "assert.sameValue(a.indexOf(10, 2), 3, 'indexOf fromIndex');\n",
            "assert.sameValue(a.indexOf(99), -1, 'indexOf missing');\n",
        ),
        "ta-indexof.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.17 %TypedArray%.prototype.join ──────────────────────────

#[test]
fn typed_array_join() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3]);\n",
            "assert.sameValue(a.join(), '1,2,3', 'default separator');\n",
            "assert.sameValue(a.join('-'), '1-2-3', 'custom separator');\n",
            "assert.sameValue(new Int32Array(0).join(), '', 'empty');\n",
        ),
        "ta-join.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.20 %TypedArray%.prototype.lastIndexOf ───────────────────

#[test]
fn typed_array_last_index_of() {
    let r = run(
        concat!(
            "var a = new Int32Array([5, 10, 15, 10]);\n",
            "assert.sameValue(a.lastIndexOf(10), 3, 'lastIndexOf 10');\n",
            "assert.sameValue(a.lastIndexOf(10, 2), 1, 'lastIndexOf fromIndex');\n",
            "assert.sameValue(a.lastIndexOf(99), -1, 'lastIndexOf missing');\n",
        ),
        "ta-lastindexof.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.21 %TypedArray%.prototype.map ───────────────────────────

#[test]
fn typed_array_map() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3]);\n",
            "var b = a.map(function(v) { return v * 2; });\n",
            "assert.sameValue(b.length, 3, 'map length');\n",
            "assert.sameValue(b[0], 2, 'map [0]');\n",
            "assert.sameValue(b[1], 4, 'map [1]');\n",
            "assert.sameValue(b[2], 6, 'map [2]');\n",
            "assert.sameValue(b instanceof Int32Array, true, 'same type');\n",
        ),
        "ta-map.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.22-23 reduce / reduceRight ──────────────────────────────

#[test]
fn typed_array_reduce() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3, 4]);\n",
            "var sum = a.reduce(function(acc, v) { return acc + v; }, 0);\n",
            "assert.sameValue(sum, 10, 'reduce sum');\n",
            "var product = a.reduceRight(function(acc, v) { return acc * v; }, 1);\n",
            "assert.sameValue(product, 24, 'reduceRight product');\n",
        ),
        "ta-reduce.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.24 %TypedArray%.prototype.reverse ───────────────────────

#[test]
fn typed_array_reverse() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3]);\n",
            "var b = a.reverse();\n",
            "assert.sameValue(a[0], 3, 'reversed [0]');\n",
            "assert.sameValue(a[1], 2, 'reversed [1]');\n",
            "assert.sameValue(a[2], 1, 'reversed [2]');\n",
            "assert.sameValue(b === a, true, 'returns same array');\n",
        ),
        "ta-reverse.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.25 %TypedArray%.prototype.set ───────────────────────────

#[test]
fn typed_array_set() {
    let r = run(
        concat!(
            "var a = new Int32Array(5);\n",
            "a.set([10, 20, 30], 1);\n",
            "assert.sameValue(a[0], 0, 'set offset [0]');\n",
            "assert.sameValue(a[1], 10, 'set offset [1]');\n",
            "assert.sameValue(a[2], 20, 'set offset [2]');\n",
            "assert.sameValue(a[3], 30, 'set offset [3]');\n",
            "assert.sameValue(a[4], 0, 'set offset [4]');\n",
        ),
        "ta-set.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_set_typed_array_source() {
    let r = run(
        concat!(
            "var src = new Float64Array([1.5, 2.5]);\n",
            "var dst = new Int32Array(4);\n",
            "dst.set(src, 1);\n",
            "assert.sameValue(dst[0], 0, 'dst[0]');\n",
            "assert.sameValue(dst[1], 1, 'dst[1] truncated');\n",
            "assert.sameValue(dst[2], 2, 'dst[2] truncated');\n",
            "assert.sameValue(dst[3], 0, 'dst[3]');\n",
        ),
        "ta-set-ta.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.26 %TypedArray%.prototype.slice ─────────────────────────

#[test]
fn typed_array_slice() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3, 4, 5]);\n",
            "var b = a.slice(1, 4);\n",
            "assert.sameValue(b.length, 3, 'slice length');\n",
            "assert.sameValue(b[0], 2, 'slice [0]');\n",
            "assert.sameValue(b[1], 3, 'slice [1]');\n",
            "assert.sameValue(b[2], 4, 'slice [2]');\n",
            "assert.sameValue(b instanceof Int32Array, true, 'same type');\n",
            "b[0] = 99;\n",
            "assert.sameValue(a[1], 2, 'independent copy');\n",
        ),
        "ta-slice.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_slice_negative() {
    let r = run(
        concat!(
            "var a = new Uint8Array([10, 20, 30, 40, 50]);\n",
            "var b = a.slice(-3);\n",
            "assert.sameValue(b.length, 3, 'slice(-3) length');\n",
            "assert.sameValue(b[0], 30, 'slice(-3) [0]');\n",
            "var c = a.slice(-4, -1);\n",
            "assert.sameValue(c.length, 3, 'slice(-4,-1) length');\n",
            "assert.sameValue(c[0], 20, 'slice(-4,-1) [0]');\n",
            "assert.sameValue(c[2], 40, 'slice(-4,-1) [2]');\n",
        ),
        "ta-slice-neg.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.27 %TypedArray%.prototype.some ──────────────────────────

#[test]
fn typed_array_some() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 3, 5]);\n",
            "assert.sameValue(a.some(function(v) { return v > 4; }), true, 'some > 4');\n",
            "assert.sameValue(a.some(function(v) { return v > 5; }), false, 'none > 5');\n",
        ),
        "ta-some.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.28 %TypedArray%.prototype.sort ──────────────────────────

#[test]
fn typed_array_sort() {
    let r = run(
        concat!(
            "var a = new Int32Array([3, 1, 4, 1, 5]);\n",
            "a.sort();\n",
            "assert.sameValue(a[0], 1, 'sort [0]');\n",
            "assert.sameValue(a[1], 1, 'sort [1]');\n",
            "assert.sameValue(a[2], 3, 'sort [2]');\n",
            "assert.sameValue(a[3], 4, 'sort [3]');\n",
            "assert.sameValue(a[4], 5, 'sort [4]');\n",
        ),
        "ta-sort.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn typed_array_sort_comparator() {
    let r = run(
        concat!(
            "var a = new Float64Array([3.1, 1.2, 4.5]);\n",
            "a.sort(function(a, b) { return b - a; });\n",
            "assert.sameValue(a[0], 4.5, 'desc [0]');\n",
            "assert.sameValue(a[1], 3.1, 'desc [1]');\n",
            "assert.sameValue(a[2], 1.2, 'desc [2]');\n",
        ),
        "ta-sort-cmp.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.29 %TypedArray%.prototype.subarray ──────────────────────

#[test]
fn typed_array_subarray() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3, 4, 5]);\n",
            "var b = a.subarray(1, 4);\n",
            "assert.sameValue(b.length, 3, 'subarray length');\n",
            "assert.sameValue(b[0], 2, 'subarray [0]');\n",
            "assert.sameValue(b[1], 3, 'subarray [1]');\n",
            "assert.sameValue(b[2], 4, 'subarray [2]');\n",
            // subarray shares underlying buffer
            "b[0] = 99;\n",
            "assert.sameValue(a[1], 99, 'shared buffer');\n",
        ),
        "ta-subarray.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.32 %TypedArray%.prototype.toString ──────────────────────

#[test]
fn typed_array_to_string() {
    let r = run(
        concat!(
            "var a = new Int32Array([1, 2, 3]);\n",
            "assert.sameValue(a.toString(), '1,2,3', 'toString');\n",
        ),
        "ta-tostring.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.33 %TypedArray%.prototype[@@toStringTag] ────────────────

#[test]
fn typed_array_to_string_tag() {
    let r = run(
        concat!(
            "assert.sameValue(Object.prototype.toString.call(new Int8Array(0)), '[object Int8Array]', 'Int8Array');\n",
            "assert.sameValue(Object.prototype.toString.call(new Uint32Array(0)), '[object Uint32Array]', 'Uint32Array');\n",
            "assert.sameValue(Object.prototype.toString.call(new Float64Array(0)), '[object Float64Array]', 'Float64Array');\n",
        ),
        "ta-tostringtag.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3 Prototype getters ────────────────────────────────────────

#[test]
fn typed_array_buffer_getter() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(16);\n",
            "var a = new Int32Array(buf);\n",
            "assert.sameValue(a.buffer === buf, true, 'buffer getter');\n",
        ),
        "ta-buffer-getter.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.2.1 %TypedArray%.from ─────────────────────────────────────

#[test]
fn typed_array_from() {
    let r = run(
        concat!(
            "var a = Int32Array.from([10, 20, 30]);\n",
            "assert.sameValue(a.length, 3, 'from length');\n",
            "assert.sameValue(a[0], 10, 'from [0]');\n",
            "assert.sameValue(a instanceof Int32Array, true, 'from type');\n",
            "var b = Uint8Array.from([1, 2, 3], function(v) { return v * 10; });\n",
            "assert.sameValue(b[0], 10, 'from mapfn [0]');\n",
            "assert.sameValue(b[1], 20, 'from mapfn [1]');\n",
        ),
        "ta-from.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.2.2 %TypedArray%.of ───────────────────────────────────────

#[test]
fn typed_array_of() {
    let r = run(
        concat!(
            "var a = Int32Array.of(10, 20, 30);\n",
            "assert.sameValue(a.length, 3, 'of length');\n",
            "assert.sameValue(a[0], 10, 'of [0]');\n",
            "assert.sameValue(a[1], 20, 'of [1]');\n",
            "assert.sameValue(a[2], 30, 'of [2]');\n",
            "assert.sameValue(a instanceof Int32Array, true, 'of type');\n",
        ),
        "ta-of.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.34 %TypedArray%.prototype.values / @@iterator ───────────

#[test]
fn typed_array_values_iterator() {
    let r = run(
        concat!(
            "var a = new Int32Array([10, 20, 30]);\n",
            "var iter = a.values();\n",
            "var r1 = iter.next();\n",
            "assert.sameValue(r1.value, 10, 'values next 0');\n",
            "assert.sameValue(r1.done, false, 'not done');\n",
            "var r2 = iter.next();\n",
            "assert.sameValue(r2.value, 20, 'values next 1');\n",
            "var r3 = iter.next();\n",
            "assert.sameValue(r3.value, 30, 'values next 2');\n",
            "var r4 = iter.next();\n",
            "assert.sameValue(r4.done, true, 'done');\n",
        ),
        "ta-values.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.18 %TypedArray%.prototype.keys ──────────────────────────

#[test]
fn typed_array_keys_iterator() {
    let r = run(
        concat!(
            "var a = new Int32Array([10, 20, 30]);\n",
            "var iter = a.keys();\n",
            "var r1 = iter.next();\n",
            "assert.sameValue(r1.value, 0, 'key 0');\n",
            "assert.sameValue(r1.done, false, 'not done');\n",
            "iter.next();\n",
            "iter.next();\n",
            "var r4 = iter.next();\n",
            "assert.sameValue(r4.done, true, 'done');\n",
        ),
        "ta-keys.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §23.2.3.6 %TypedArray%.prototype.entries ────────────────────────

#[test]
fn typed_array_entries_iterator() {
    let r = run(
        concat!(
            "var a = new Int32Array([10, 20]);\n",
            "var iter = a.entries();\n",
            "var r1 = iter.next();\n",
            "assert.sameValue(r1.value[0], 0, 'entry key');\n",
            "assert.sameValue(r1.value[1], 10, 'entry value');\n",
            "assert.sameValue(r1.done, false, 'not done');\n",
            "var r2 = iter.next();\n",
            "assert.sameValue(r2.value[0], 1, 'entry key 1');\n",
            "assert.sameValue(r2.value[1], 20, 'entry value 1');\n",
            "var r3 = iter.next();\n",
            "assert.sameValue(r3.done, true, 'done');\n",
        ),
        "ta-entries.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── for-of loop with TypedArray ─────────────────────────────────────

#[test]
fn typed_array_for_of() {
    let r = run(
        concat!(
            "var a = new Int32Array([10, 20, 30]);\n",
            "var sum = 0;\n",
            "for (var v of a) { sum += v; }\n",
            "assert.sameValue(sum, 60, 'for-of sum');\n",
        ),
        "ta-for-of.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── instanceof checks ───────────────────────────────────────────────

#[test]
fn typed_array_instanceof() {
    let r = run(
        concat!(
            "var a = new Uint16Array(2);\n",
            "assert.sameValue(a instanceof Uint16Array, true, 'instanceof concrete');\n",
        ),
        "ta-instanceof.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── BYTES_PER_ELEMENT on constructor and prototype ──────────────────

#[test]
fn typed_array_bytes_per_element() {
    let r = run(
        concat!(
            "assert.sameValue(Int8Array.BYTES_PER_ELEMENT, 1, 'Int8 ctor');\n",
            "assert.sameValue(Int16Array.BYTES_PER_ELEMENT, 2, 'Int16 ctor');\n",
            "assert.sameValue(Int32Array.BYTES_PER_ELEMENT, 4, 'Int32 ctor');\n",
            "assert.sameValue(Float64Array.BYTES_PER_ELEMENT, 8, 'Float64 ctor');\n",
            "assert.sameValue(new Uint32Array(0).BYTES_PER_ELEMENT, 4, 'instance');\n",
        ),
        "ta-bpe.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

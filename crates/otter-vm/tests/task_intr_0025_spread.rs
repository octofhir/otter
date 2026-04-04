//! Integration tests for spread & rest syntax.
//!
//! Spec references:
//! - §13.2.4.1 ArrayAccumulation:           <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
//! - §13.3.8.1 ArgumentListEvaluation:      <https://tc39.es/ecma262/#sec-runtime-semantics-argumentlistevaluation>
//! - §13.3.5   The `new` Operator:          <https://tc39.es/ecma262/#sec-new-operator>
//! - §12.3.7.1 SuperCall:                   <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, source_url)
        .expect("test262 basic script should compile on the new VM path");

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
        .expect("test262 basic script should execute on the new VM path")
        .return_value()
}

// ═══════════════════════════════════════════════════════════════════════════
//  Array Spread — §13.2.4.1
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn array_spread_basic() {
    let result = execute_test262_basic(
        concat!(
            "var a = [1, 2, 3];\n",
            "var b = [...a];\n",
            "assert.sameValue(b.length, 3, 'spread copies length');\n",
            "assert.sameValue(b[0], 1, 'element 0');\n",
            "assert.sameValue(b[1], 2, 'element 1');\n",
            "assert.sameValue(b[2], 3, 'element 2');\n",
        ),
        "array-spread-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_concat() {
    let result = execute_test262_basic(
        concat!(
            "var a = [1, 2];\n",
            "var b = [3, 4];\n",
            "var c = [...a, ...b];\n",
            "assert.sameValue(c.length, 4, 'two spreads');\n",
            "assert.sameValue(c[0], 1, 'c[0]');\n",
            "assert.sameValue(c[1], 2, 'c[1]');\n",
            "assert.sameValue(c[2], 3, 'c[2]');\n",
            "assert.sameValue(c[3], 4, 'c[3]');\n",
        ),
        "array-spread-concat.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_mixed() {
    let result = execute_test262_basic(
        concat!(
            "var a = [2, 3];\n",
            "var b = [1, ...a, 4, 5];\n",
            "assert.sameValue(b.length, 5, 'mixed spread');\n",
            "assert.sameValue(b[0], 1, 'b[0]');\n",
            "assert.sameValue(b[1], 2, 'b[1]');\n",
            "assert.sameValue(b[2], 3, 'b[2]');\n",
            "assert.sameValue(b[3], 4, 'b[3]');\n",
            "assert.sameValue(b[4], 5, 'b[4]');\n",
        ),
        "array-spread-mixed.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_string() {
    let result = execute_test262_basic(
        concat!(
            "var a = [...'abc'];\n",
            "assert.sameValue(a.length, 3, 'string spread length');\n",
            "assert.sameValue(a[0], 'a', 'a[0]');\n",
            "assert.sameValue(a[1], 'b', 'a[1]');\n",
            "assert.sameValue(a[2], 'c', 'a[2]');\n",
        ),
        "array-spread-string.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_empty() {
    let result = execute_test262_basic(
        concat!(
            "var a = [...[]];\n",
            "assert.sameValue(a.length, 0, 'spread empty array');\n",
            "var b = [1, ...[], 2];\n",
            "assert.sameValue(b.length, 2, 'spread empty in middle');\n",
            "assert.sameValue(b[0], 1, 'b[0]');\n",
            "assert.sameValue(b[1], 2, 'b[1]');\n",
        ),
        "array-spread-empty.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_is_shallow_copy() {
    let result = execute_test262_basic(
        concat!(
            "var original = [1, 2, 3];\n",
            "var copy = [...original];\n",
            "copy[0] = 99;\n",
            "assert.sameValue(original[0], 1, 'original unmodified');\n",
            "assert.sameValue(copy[0], 99, 'copy modified');\n",
        ),
        "array-spread-copy.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_with_set_iterator() {
    let result = execute_test262_basic(
        concat!(
            "var s = new Set([3, 1, 4, 1, 5]);\n",
            "var a = [...s];\n",
            "assert.sameValue(a.length, 4, 'Set deduplicates');\n",
            "assert.sameValue(a[0], 3, 'insertion order 0');\n",
            "assert.sameValue(a[1], 1, 'insertion order 1');\n",
            "assert.sameValue(a[2], 4, 'insertion order 2');\n",
            "assert.sameValue(a[3], 5, 'insertion order 3');\n",
        ),
        "array-spread-set.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_with_map_entries() {
    let result = execute_test262_basic(
        concat!(
            "var m = new Map([['a', 1], ['b', 2]]);\n",
            "var a = [...m];\n",
            "assert.sameValue(a.length, 2, 'Map spread length');\n",
            "assert.sameValue(a[0][0], 'a', 'key 0');\n",
            "assert.sameValue(a[0][1], 1, 'value 0');\n",
            "assert.sameValue(a[1][0], 'b', 'key 1');\n",
            "assert.sameValue(a[1][1], 2, 'value 1');\n",
        ),
        "array-spread-map.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_spread_with_generator() {
    let result = execute_test262_basic(
        concat!(
            "function* gen() { yield 10; yield 20; yield 30; }\n",
            "var a = [...gen()];\n",
            "assert.sameValue(a.length, 3, 'generator spread length');\n",
            "assert.sameValue(a[0], 10, 'a[0]');\n",
            "assert.sameValue(a[1], 20, 'a[1]');\n",
            "assert.sameValue(a[2], 30, 'a[2]');\n",
        ),
        "array-spread-generator.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Call Spread — §13.3.8.1
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn call_spread_basic() {
    let result = execute_test262_basic(
        concat!(
            "function sum(a, b, c) { return a + b + c; }\n",
            "var args = [1, 2, 3];\n",
            "assert.sameValue(sum(...args), 6, 'spread call');\n",
        ),
        "call-spread-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn call_spread_mixed_args() {
    let result = execute_test262_basic(
        concat!(
            "function f(a, b, c, d) { return '' + a + b + c + d; }\n",
            "var mid = [2, 3];\n",
            "assert.sameValue(f(1, ...mid, 4), '1234', 'mixed spread');\n",
        ),
        "call-spread-mixed.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn call_spread_multiple_spreads() {
    let result = execute_test262_basic(
        concat!(
            "function f() {\n",
            "  var sum = 0;\n",
            "  for (var i = 0; i < arguments.length; i++) sum += arguments[i];\n",
            "  return sum;\n",
            "}\n",
            "var a = [1, 2];\n",
            "var b = [3, 4];\n",
            "assert.sameValue(f(...a, ...b), 10, 'multiple spreads');\n",
        ),
        "call-spread-multiple.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn call_spread_empty_array() {
    let result = execute_test262_basic(
        concat!(
            "function f() { return arguments.length; }\n",
            "assert.sameValue(f(...[]), 0, 'empty spread gives 0 args');\n",
        ),
        "call-spread-empty.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn call_spread_string_iterable() {
    let result = execute_test262_basic(
        concat!(
            "function f(a, b, c) { return a + b + c; }\n",
            "assert.sameValue(f(...'xyz'), 'xyz', 'spread string into call');\n",
        ),
        "call-spread-string.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn call_spread_method() {
    let result = execute_test262_basic(
        concat!(
            "var obj = {\n",
            "  sum: function(a, b) { return a + b; }\n",
            "};\n",
            "var args = [10, 20];\n",
            "assert.sameValue(obj.sum(...args), 30, 'method spread');\n",
        ),
        "call-spread-method.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn call_spread_math_max() {
    let result = execute_test262_basic(
        concat!(
            "var nums = [5, 2, 8, 1, 9];\n",
            "assert.sameValue(Math.max(...nums), 9, 'Math.max spread');\n",
        ),
        "call-spread-math-max.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  New Spread — §13.3.5
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn new_spread_basic() {
    let result = execute_test262_basic(
        concat!(
            "function Pair(a, b) { this.a = a; this.b = b; }\n",
            "var args = [10, 20];\n",
            "var p = new Pair(...args);\n",
            "assert.sameValue(p.a, 10, 'new spread a');\n",
            "assert.sameValue(p.b, 20, 'new spread b');\n",
            "assert.sameValue(p instanceof Pair, true, 'instanceof');\n",
        ),
        "new-spread-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn new_spread_array_constructor() {
    let result = execute_test262_basic(
        concat!(
            "var elems = [1, 2, 3];\n",
            "var a = new Array(...elems);\n",
            "assert.sameValue(a.length, 3, 'new Array spread length');\n",
            "assert.sameValue(a[0], 1, 'a[0]');\n",
            "assert.sameValue(a[1], 2, 'a[1]');\n",
            "assert.sameValue(a[2], 3, 'a[2]');\n",
        ),
        "new-spread-array.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn new_spread_mixed() {
    let result = execute_test262_basic(
        concat!(
            "function Triple(a, b, c) { this.a = a; this.b = b; this.c = c; }\n",
            "var rest = [2, 3];\n",
            "var t = new Triple(1, ...rest);\n",
            "assert.sameValue(t.a, 1, 'first arg');\n",
            "assert.sameValue(t.b, 2, 'spread arg 0');\n",
            "assert.sameValue(t.c, 3, 'spread arg 1');\n",
        ),
        "new-spread-mixed.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Super Spread — §12.3.7.1
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn super_spread_basic() {
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  constructor(a, b) { this.a = a; this.b = b; }\n",
            "}\n",
            "class Child extends Base {\n",
            "  constructor() {\n",
            "    var args = [10, 20];\n",
            "    super(...args);\n",
            "  }\n",
            "}\n",
            "var c = new Child();\n",
            "assert.sameValue(c.a, 10, 'super spread a');\n",
            "assert.sameValue(c.b, 20, 'super spread b');\n",
            "assert.sameValue(c instanceof Child, true, 'instanceof Child');\n",
            "assert.sameValue(c instanceof Base, true, 'instanceof Base');\n",
        ),
        "super-spread-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn super_spread_mixed() {
    // super(first, ...rest) — mixed positional + spread args.
    // NOTE: `this.sum = a + b + c` is a pre-existing class constructor expression
    // bug (register clobbering with `+` in constructors). Test individual properties.
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  constructor(a, b, c) { this.a = a; this.b = b; this.c = c; }\n",
            "}\n",
            "class Child extends Base {\n",
            "  constructor(first) {\n",
            "    var rest = [2, 3];\n",
            "    super(first, ...rest);\n",
            "  }\n",
            "}\n",
            "var c = new Child(1);\n",
            "assert.sameValue(c.a, 1, 'super mixed spread a');\n",
            "assert.sameValue(c.b, 2, 'super mixed spread b');\n",
            "assert.sameValue(c.c, 3, 'super mixed spread c');\n",
        ),
        "super-spread-mixed.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Rest Parameters + Spread Interaction
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn rest_and_spread_roundtrip() {
    let result = execute_test262_basic(
        concat!(
            "function collect(...args) { return args; }\n",
            "var a = collect(...[1, 2, 3]);\n",
            "assert.sameValue(a.length, 3, 'rest collects spread args');\n",
            "assert.sameValue(a[0], 1, 'a[0]');\n",
            "assert.sameValue(a[1], 2, 'a[1]');\n",
            "assert.sameValue(a[2], 3, 'a[2]');\n",
        ),
        "rest-spread-roundtrip.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn spread_preserves_argument_order() {
    let result = execute_test262_basic(
        concat!(
            "function f() {\n",
            "  var result = '';\n",
            "  for (var i = 0; i < arguments.length; i++) {\n",
            "    result += arguments[i];\n",
            "  }\n",
            "  return result;\n",
            "}\n",
            "assert.sameValue(f('a', ...'bc', 'd'), 'abcd', 'order preserved');\n",
        ),
        "spread-order.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Edge Cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spread_into_push() {
    let result = execute_test262_basic(
        concat!(
            "var a = [1, 2, 3];\n",
            "var b = [4, 5];\n",
            "a.push(...b);\n",
            "assert.sameValue(a.length, 5, 'push spread length');\n",
            "assert.sameValue(a[3], 4, 'a[3]');\n",
            "assert.sameValue(a[4], 5, 'a[4]');\n",
        ),
        "spread-push.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn spread_nested_arrays() {
    let result = execute_test262_basic(
        concat!(
            "var inner = [2, 3];\n",
            "var outer = [1, ...[...inner, 4], 5];\n",
            "assert.sameValue(outer.length, 5, 'nested spread');\n",
            "assert.sameValue(outer[0], 1, 'o[0]');\n",
            "assert.sameValue(outer[1], 2, 'o[1]');\n",
            "assert.sameValue(outer[2], 3, 'o[2]');\n",
            "assert.sameValue(outer[3], 4, 'o[3]');\n",
            "assert.sameValue(outer[4], 5, 'o[4]');\n",
        ),
        "spread-nested.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ══���═══════════════════════���═════════════════════════════���══════════════════
//  Constructor expression bug diagnosis
// ════════════════════════════════���═══════════════════════════════��══════════

// ═══════════════════════════════════════════════════════════════════════════
//  Constructor expression tests (previously broken, now fixed)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn ctor_this_property_with_expression() {
    // Previously broken: this.sum = a + b clobbered `this` temp register.
    let result = execute_test262_basic(
        concat!(
            "function Base(a, b) { this.sum = a + b; }\n",
            "var x = new Base(10, 20);\n",
            "assert.sameValue(x.sum, 30, 'function ctor a+b');\n",
        ),
        "ctor-expr-fn.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_ctor_this_property_with_expression() {
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  constructor(a, b) { this.sum = a + b; }\n",
            "}\n",
            "var x = new Base(10, 20);\n",
            "assert.sameValue(x.sum, 30, 'class ctor a+b');\n",
        ),
        "ctor-expr-class.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_ctor_super_with_expression() {
    // Previously broken: super(first, ...rest) + this.sum = a+b+c
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  constructor(a, b, c) { this.sum = a + b + c; }\n",
            "}\n",
            "class Child extends Base {\n",
            "  constructor(first) {\n",
            "    var rest = [2, 3];\n",
            "    super(first, ...rest);\n",
            "  }\n",
            "}\n",
            "var c = new Child(1);\n",
            "assert.sameValue(c.sum, 6, 'super mixed spread with expression');\n",
            "assert.sameValue(c instanceof Child, true, 'instanceof Child');\n",
            "assert.sameValue(c instanceof Base, true, 'instanceof Base');\n",
        ),
        "ctor-super-expr.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

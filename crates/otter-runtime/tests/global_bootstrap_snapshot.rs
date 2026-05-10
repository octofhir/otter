//! Snapshot of the default `globalThis` shape after bootstrap.
//!
//! This is the P1.3 anchor test: any change to the global object
//! shape — new globals, reordered installs, attribute drift,
//! prototype-identity drift — surfaces as a string diff in this
//! single fixture. Reviewers can read the diff and decide whether
//! the change is intentional.
//!
//! # Contents
//! - `globalThis_default_snapshot` — `(name, writable, enumerable,
//!   configurable, value-kind)` for every own property.
//! - `global_constructor_prototype_identity` — pinned prototype
//!   chain identity for every realm constructor.
//!
//! # Invariants
//! - Snapshot contents must stay deterministic across runs.
//! - Adding a new global means updating the snapshot in the same
//!   slice.
//!
//! # See also
//! - `ENGINE_REFACTOR_EXECUTION_PLAN.md` P1.3.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<test>")
        .expect("script")
        .completion_string()
        .to_string()
}

/// Pin every default `globalThis` own property descriptor. The
/// payload is a JS-side dump of `(name, writable, enumerable,
/// configurable, typeof)` so descriptor drift is visible at review
/// time.
#[test]
fn global_this_default_snapshot() {
    let dump = run(r#"
        const ownStrings = Object.getOwnPropertyNames(globalThis).sort();
        const lines = ownStrings.map(name => {
            const d = Object.getOwnPropertyDescriptor(globalThis, name);
            const kind = d.get || d.set
                ? "accessor"
                : (d.value === null ? "null" : typeof d.value);
            const writable = d.writable === undefined ? "-" : (d.writable ? "w" : ".");
            const enumerable = d.enumerable ? "e" : ".";
            const configurable = d.configurable ? "c" : ".";
            return name + " " + writable + enumerable + configurable + " " + kind;
        });
        lines.join("\n");
        "#);
    // Snapshot of current realm. Drift discussion:
    //
    // - Every default global is `{ writable: true, enumerable: false,
    //   configurable: true }` per §17 / §19.{2,4}. The only enumerable
    //   own property is `globalThis` itself (§19.4.1).
    // - Foundation gap: `Array` / `Object` / `Function` / `Number` /
    //   `Boolean` / `String` / `JSON` / `Math` etc. land as
    //   `Value::Object` (`typeof === "object"`). The native-error
    //   stack and dynamic-`new`-able callables (`Error` family,
    //   `Date`, `Proxy`, …) carry a real `[[Call]]` slot via
    //   `set_constructor_native` and report `typeof === "function"`.
    //   Aligning the bare-Object constructors to function-typed
    //   values is filed against the remaining P1.2 metadata work.
    // - Every placeholder (`BigInt`, `RegExp`, `Map`, `Set`, …)
    //   surfaces today through the placeholder install pipeline.
    let expected = "\
AggregateError w.c function
Array w.c function
ArrayBuffer w.c object
Atomics w.c object
BigInt w.c object
BigInt64Array w.c object
BigUint64Array w.c object
Boolean w.c function
DataView w.c object
Date w.c function
Error w.c function
EvalError w.c function
FinalizationRegistry w.c object
Float32Array w.c object
Float64Array w.c object
Function w.c function
Int16Array w.c object
Int32Array w.c object
Int8Array w.c object
Intl w.c object
Iterator w.c object
JSON w.c object
Map w.c object
Math w.c object
Number w.c function
Object w.c function
Promise w.c object
Proxy w.c function
RangeError w.c function
ReferenceError w.c function
Reflect w.c object
RegExp w.c object
Set w.c object
SharedArrayBuffer w.c object
String w.c function
Symbol w.c object
SyntaxError w.c function
Temporal w.c object
TypeError w.c function
URIError w.c function
Uint16Array w.c object
Uint32Array w.c object
Uint8Array w.c object
Uint8ClampedArray w.c object
WeakMap w.c object
WeakRef w.c object
WeakSet w.c object
clearInterval w.c function
clearTimeout w.c function
console w.c object
globalThis wec object
setInterval w.c function
setTimeout w.c function";
    assert_eq!(dump, expected, "default globalThis own properties drifted");
}

/// Pin every realm constructor's `[[Prototype]]` identity and
/// `.prototype.[[Prototype]]` so accidental rewrites in one
/// installer don't silently change the inheritance chain.
#[test]
fn global_constructor_prototype_identity() {
    let dump = run(r#"
        function probe(name) {
            const ctor = globalThis[name];
            if (typeof ctor !== "function" && typeof ctor !== "object") return name + " missing";
            const ctorProto = Object.getPrototypeOf(ctor);
            const protoProto = ctor.prototype && Object.getPrototypeOf(ctor.prototype);
            const fp = ctorProto === Function.prototype ? "FP" : (ctorProto === Error ? "Error" : (ctorProto === null ? "null" : "?"));
            const pp = protoProto === Object.prototype ? "OP"
                : protoProto === Error.prototype ? "EP"
                : protoProto === null ? "null" : "?";
            return name + " ctor=>" + fp + " proto.proto=>" + pp;
        }
        const ctors = [
            "Object", "Function", "Array", "Number", "Boolean", "String",
            "Error", "TypeError", "RangeError", "SyntaxError",
            "ReferenceError", "URIError", "EvalError", "AggregateError",
        ];
        ctors.map(probe).join("\n");
        "#);
    let expected = "\
Object ctor=>FP proto.proto=>null
Function ctor=>FP proto.proto=>OP
Array ctor=>FP proto.proto=>OP
Number ctor=>FP proto.proto=>OP
Boolean ctor=>FP proto.proto=>OP
String ctor=>FP proto.proto=>OP
Error ctor=>FP proto.proto=>OP
TypeError ctor=>Error proto.proto=>EP
RangeError ctor=>Error proto.proto=>EP
SyntaxError ctor=>Error proto.proto=>EP
ReferenceError ctor=>Error proto.proto=>EP
URIError ctor=>Error proto.proto=>EP
EvalError ctor=>Error proto.proto=>EP
AggregateError ctor=>Error proto.proto=>EP";
    assert_eq!(dump, expected, "constructor prototype identity drifted");
}

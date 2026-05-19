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
    // - Constructors that carry a real `[[Call]]` / `[[Construct]]`
    //   surface report `typeof === "function"`; namespace objects
    //   and internal prototype markers remain object-typed.
    let expected = "\
@@%TypedArray% ... function
@@%TypedArrayPrototype% ... object
AggregateError w.c function
Array w.c function
ArrayBuffer w.c function
Atomics w.c object
BigInt w.c function
BigInt64Array w.c function
BigUint64Array w.c function
Boolean w.c function
DataView w.c function
Date w.c function
Error w.c function
EvalError w.c function
FinalizationRegistry w.c function
Float32Array w.c function
Float64Array w.c function
Function w.c function
Infinity ... number
Int16Array w.c function
Int32Array w.c function
Int8Array w.c function
Intl w.c object
Iterator w.c function
JSON w.c object
Map w.c function
Math w.c object
NaN ... number
Number w.c function
Object w.c function
Promise w.c function
Proxy w.c function
RangeError w.c function
ReferenceError w.c function
Reflect w.c object
RegExp w.c function
Set w.c function
SharedArrayBuffer w.c function
String w.c function
Symbol w.c function
SyntaxError w.c function
Temporal w.c object
TypeError w.c function
URIError w.c function
Uint16Array w.c function
Uint32Array w.c function
Uint8Array w.c function
Uint8ClampedArray w.c function
WeakMap w.c function
WeakRef w.c function
WeakSet w.c function
clearInterval w.c function
clearTimeout w.c function
console w.c object
decodeURI w.c function
decodeURIComponent w.c function
encodeURI w.c function
encodeURIComponent w.c function
escape w.c function
eval w.c function
globalThis wec object
isFinite w.c function
isNaN w.c function
parseFloat w.c function
parseInt w.c function
process w.c object
setInterval w.c function
setTimeout w.c function
undefined ... undefined
unescape w.c function";
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

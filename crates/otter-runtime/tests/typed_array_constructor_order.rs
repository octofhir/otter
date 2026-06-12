//! Runtime regression coverage for TypedArray constructor ordering.
//!
//! # Contents
//! - Argument `ToIndex` errors precede `newTarget.prototype` lookup.
//!
//! # Invariants
//! - TypedArray constructors allocate their own exotic bodies in native code;
//!   generic construct dispatch must not eagerly run
//!   `GetPrototypeFromConstructor`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-typedarray>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<typed-array-constructor-order>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn length_type_error_precedes_new_target_prototype_lookup() {
    let completion = run(r#"
        let touched = false;
        const newTarget = Object.defineProperty(function() {}.bind(null), "prototype", {
            get: function() {
                touched = true;
                return {};
            }
        });
        try {
            Reflect.construct(Float64Array, [Symbol()], newTarget);
            "no throw";
        } catch (e) {
            e.name + ":" + touched;
        }
        "#);
    assert_eq!(completion, "TypeError:false");
}

#[test]
fn constructor_rejects_out_of_bounds_typed_array_source() {
    let completion = run(r#"
        const rab = new ArrayBuffer(4, { maxByteLength: 8 });
        const source = new Int8Array(rab, 0, 4);
        rab.resize(3);
        try {
            new Int8Array(source);
            "no throw";
        } catch (e) {
            e.name;
        }
        "#);
    assert_eq!(completion, "TypeError");
}

#[test]
fn iterable_constructor_observes_custom_array_iterator_next() {
    let completion = run(r#"
        const proto = Object.getPrototypeOf([][Symbol.iterator]());
        const original = proto.next;
        let calls = 0;
        proto.next = function() {
            calls++;
            return original.call(this);
        };
        try {
            const source = {
                [Symbol.iterator]: function() {
                    return [1, 2, 3][Symbol.iterator]();
                }
            };
            const out = new Uint8Array(source);
            calls === 4 && out.length === 3 && out[2] === 3;
        } finally {
            proto.next = original;
        }
        "#);
    assert_eq!(completion, "true");
}

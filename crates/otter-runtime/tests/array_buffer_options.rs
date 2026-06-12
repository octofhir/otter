//! Runtime regression coverage for ArrayBuffer constructor option ordering.
//!
//! # Contents
//! - Observable `maxByteLength` option reads.
//! - NewTarget prototype lookup ordering around allocation validation.
//!
//! # Invariants
//! - `maxByteLength` accessors and coercions run before allocation.
//! - `byteLength > maxByteLength` throws before `newTarget.prototype`.
//! - Fixed-length allocation reaches `newTarget.prototype` before data-block
//!   allocation failure.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn array_buffer_max_byte_length_getter_throw_is_preserved() {
    let completion = run(r#"
        function Boom() {}
        const options = {
            get maxByteLength() {
                throw new Boom();
            }
        };
        try {
            new ArrayBuffer(0, options);
            "no throw";
        } catch (e) {
            e instanceof Boom;
        }
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn array_buffer_max_less_than_length_precedes_new_target_prototype() {
    let completion = run(r#"
        let touched = false;
        const newTarget = Object.defineProperty(function() {}.bind(null), "prototype", {
            get: function() {
                touched = true;
                return {};
            }
        });
        try {
            Reflect.construct(ArrayBuffer, [10, { maxByteLength: 0 }], newTarget);
            "no throw";
        } catch (e) {
            e.name + ":" + touched;
        }
        "#);
    assert_eq!(completion, "RangeError:false");
}

#[test]
fn array_buffer_fixed_allocation_observes_new_target_before_data_allocation() {
    let completion = run(r#"
        function DummyError() {}
        const newTarget = Object.defineProperty(function() {}.bind(null), "prototype", {
            get: function() {
                throw new DummyError();
            }
        });
        try {
            Reflect.construct(ArrayBuffer, [7 * Math.pow(1024, 5)], newTarget);
            "no throw";
        } catch (e) {
            e instanceof DummyError;
        }
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn shared_array_buffer_max_byte_length_options_are_observable() {
    let completion = run(r#"
        let touched = false;
        const newTarget = Object.defineProperty(function() {}.bind(null), "prototype", {
            get: function() {
                touched = true;
                return {};
            }
        });
        const range = (() => {
            try {
                Reflect.construct(SharedArrayBuffer, [10, { maxByteLength: 0 }], newTarget);
                return "no throw";
            } catch (e) {
                return e.name + ":" + touched;
            }
        })();
        const fixed = new SharedArrayBuffer(0, { maxByteLength: undefined }).growable;
        range + ":" + fixed;
        "#);
    assert_eq!(completion, "RangeError:false:false");
}

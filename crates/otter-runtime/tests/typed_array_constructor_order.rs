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

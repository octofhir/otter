//! Runtime regression coverage for DataView constructor ordering.
//!
//! # Contents
//! - `byteOffset` validation precedes `newTarget.prototype` lookup.
//!
//! # Invariants
//! - `DataView` allocates its own exotic body in the native constructor; generic
//!   construct dispatch must not eagerly run `GetPrototypeFromConstructor`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-dataview-buffer-byteoffset-bytelength>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<data-view-constructor-order>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn byte_offset_range_error_precedes_new_target_prototype_lookup() {
    let completion = run(r#"
        let touched = false;
        const newTarget = Object.defineProperty(function() {}.bind(null), "prototype", {
            get: function() {
                touched = true;
                return {};
            }
        });
        try {
            Reflect.construct(DataView, [new ArrayBuffer(0), 10], newTarget);
            "no throw";
        } catch (e) {
            e.name + ":" + touched;
        }
        "#);
    assert_eq!(completion, "RangeError:false");
}

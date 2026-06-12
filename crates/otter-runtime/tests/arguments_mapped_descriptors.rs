//! Runtime regression coverage for mapped arguments descriptors.
//!
//! # Contents
//! - Sloppy mapped arguments interaction with `Object.defineProperty`.
//!
//! # Invariants
//! - Removing a parameter map through `writable:false` preserves the
//!   current parameter value in the ordinary own data property.
//! - Descriptor attributes set before unmapping survive the final
//!   descriptor update.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arguments-exotic-objects-defineownproperty-p-desc>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<arguments-mapped-descriptors>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn writable_false_without_value_captures_current_parameter_value() {
    let completion = run(r#"
        function f(a) {
            Object.defineProperty(arguments, "0", { configurable: false });
            a = 2;
            Object.defineProperty(arguments, "0", { writable: false });
            const before = arguments[0];
            a = 3;
            return before + ":" + arguments[0] + ":" + a;
        }
        f(1);
        "#);
    assert_eq!(completion, "2:2:3");
}

#[test]
fn descriptor_attributes_survive_parameter_map_removal() {
    let completion = run(r#"
        function f(a) {
            Object.defineProperty(arguments, "0", {
                configurable: false,
                enumerable: false
            });
            a = 2;
            Object.defineProperty(arguments, "0", { writable: false });
            const desc = Object.getOwnPropertyDescriptor(arguments, "0");
            a = 3;
            return desc.value + ":" + desc.writable + ":" +
                desc.enumerable + ":" + desc.configurable + ":" + arguments[0];
        }
        f(1);
        "#);
    assert_eq!(completion, "2:false:false:false:2");
}

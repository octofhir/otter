//! Runtime regression coverage for `%Array.prototype%` as an Array exotic.
//!
//! # Contents
//! - `%Array.prototype%` reports as an Array.
//! - Index writes update `%Array.prototype.length`.
//! - `Object.prototype.toString` observes the Array brand.
//!
//! # Invariants
//! - `%Array.prototype%` is installed as an ordinary object today, so VM
//!   intrinsic paths must preserve the externally observable Array exotic bits.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-prototype-exotic>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn array_prototype_exposes_array_exotic_observables() {
    let completion = run(r#"
        Array.prototype[2] = 42;
        [
            Array.isArray(Array.prototype),
            Array.prototype.length,
            Array.prototype[0] === undefined,
            Array.prototype[1] === undefined,
            Array.prototype[2],
            Object.prototype.toString.call(Array.prototype),
        ].join("|");
        "#);
    assert_eq!(completion, "true|3|true|true|42|[object Array]");
}

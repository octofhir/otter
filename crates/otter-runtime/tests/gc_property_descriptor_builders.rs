//! Moving-GC invariants for property-descriptor result builders.
//!
//! # Contents
//! - Partial descriptors passed to a Proxy `defineProperty` trap.
//!
//! # Invariants
//! - Proxy trap descriptor objects preserve exactly the optional fields and
//!   values supplied by the caller across moving collections.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str, name: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("property descriptor fixture")
        .completion_string()
        .to_owned()
}

#[test]
fn proxy_partial_descriptor_survives_each_field_write() {
    let completion = run(
        r#"
        let observed;
        const target = {};
        const proxy = new Proxy(target, {
            defineProperty(inner, key, descriptor) {
                observed = descriptor;
                return Reflect.defineProperty(inner, key, descriptor);
            }
        });
        const payload = { marker: 73 };
        const success = Reflect.defineProperty(proxy, "value", {
            value: payload,
            writable: true,
            configurable: true
        });
        success === true &&
            observed.value === payload &&
            observed.writable === true &&
            observed.configurable === true &&
            !("enumerable" in observed);
        "#,
        "<gc-partial-property-descriptor>",
    );

    assert_eq!(completion, "true");
}

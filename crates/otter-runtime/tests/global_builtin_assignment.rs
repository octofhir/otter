//! Runtime regression coverage for writable builtin global bindings.
//!
//! # Contents
//! - Assignment to a builtin constructor global updates the global object.
//!
//! # Invariants
//! - ECMAScript builtin constructors are writable, non-enumerable,
//!   configurable global properties.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-value-properties-of-the-global-object>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<global-builtin-assignment>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn symbol_constructor_global_is_writable() {
    let completion = run(r#"
        Symbol = undefined;
        String(Symbol === undefined) + ":" + String(globalThis.Symbol === undefined);
        "#);
    assert_eq!(completion, "true:true");
}

#[test]
fn property_read_on_clobbered_symbol_global_throws() {
    let completion = run(r#"
        Symbol = undefined;
        let threw = false;
        try {
            Symbol.iterator;
        } catch (error) {
            threw = error instanceof TypeError;
        }
        String(threw);
        "#);
    assert_eq!(completion, "true");
}

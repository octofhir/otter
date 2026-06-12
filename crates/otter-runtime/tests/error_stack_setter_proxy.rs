//! Runtime regression coverage for `Error.prototype.stack` setter proxies.
//!
//! # Contents
//! - Proxy receivers in the Error Stacks proposal setter algorithm.
//!
//! # Invariants
//! - The setter's existing-own-property path delegates to `Set` with
//!   `Throw=true` instead of recursively calling the inherited setter.
//! - The no-own-property path delegates to `CreateDataPropertyOrThrow`.
//!
//! # See also
//! - <https://tc39.es/proposal-error-stacks/#sec-set-error.prototype.stack>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<error-stack-setter-proxy>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn proxy_wrapping_error_prototype_uses_set_trap() {
    let completion = run(r#"
        const setter = Object.getOwnPropertyDescriptor(Error.prototype, "stack").set;
        let log = "";
        const proxy = new Proxy(Error.prototype, {
            getOwnPropertyDescriptor(target, key) {
                log += "g:" + key + ";";
                return Object.getOwnPropertyDescriptor(target, key);
            },
            set(_target, key, value) {
                log += "s:" + key + ":" + value + ";";
                return true;
            }
        });
        setter.call(proxy, "sentinel");
        log;
        "#);
    assert_eq!(completion, "g:stack;s:stack:sentinel;");
}

#[test]
fn proxy_without_own_stack_uses_define_property_trap() {
    let completion = run(r#"
        const setter = Object.getOwnPropertyDescriptor(Error.prototype, "stack").set;
        let log = "";
        const target = {};
        const proxy = new Proxy(target, {
            getOwnPropertyDescriptor(target, key) {
                log += "g:" + key + ";";
                return Object.getOwnPropertyDescriptor(target, key);
            },
            defineProperty(target, key, desc) {
                log += "d:" + key + ":" + desc.value + ":" + desc.writable + ";";
                return Reflect.defineProperty(target, key, desc);
            }
        });
        setter.call(proxy, "sentinel");
        log + target.stack;
        "#);
    assert_eq!(completion, "g:stack;d:stack:sentinel:true;sentinel");
}

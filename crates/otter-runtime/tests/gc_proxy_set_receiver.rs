//! Moving-GC coverage for Proxy `[[Set]]` trap arguments.
//!
//! # Contents
//! - A Proxy reached through an ordinary prototype chain.
//! - A trap that forwards through `Reflect.set` with the original receiver.
//!
//! # Invariants
//! - Key allocation cannot stale the proxy, value, or receiver arguments.
//! - The forwarded write creates the property on the original receiver.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn proxy_set_key_allocation_preserves_the_receiver_on_every_tier() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
const prototype = {};
let traps = 0;
Object.setPrototypeOf(prototype, new Proxy({}, {
  set(target, key, value, receiver) {
    traps++;
    return Reflect.set(target, key, value, receiver);
  }
}));
const receiver = Object.create(prototype);
receiver.payload = 73;
JSON.stringify([traps, receiver.payload, Object.hasOwn(receiver, "payload")]);
"#,
                ),
                "step10-proxy-set-receiver.js",
            )
            .expect("Proxy set receiver");
        assert_eq!(result.completion_string(), "[1,73,true]", "{selection:?}");
    }
}

//! Runtime regression coverage for `Array.prototype.reverse` MOP order.
//!
//! # Contents
//! - Lower index `Get` happens before upper index `Has`.
//! - Huge array-like receivers use the same lower/upper operation order.
//!
//! # Invariants
//! - The reverse driver keeps `LengthOfArrayLike` fixed but performs
//!   live `Has`, `Get`, `Set`, and `Delete` operations in spec order.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.prototype.reverse>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-reverse-order>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn reverse_gets_lower_before_testing_upper() {
    let completion = run(r#"
        const array = ["first", "second"];
        Object.defineProperty(array, "0", {
            get() {
                array.length = 0;
                return "first";
            },
        });
        array.reverse();
        (0 in array) + "|" + (1 in array) + "|" + array[1];
        "#);
    assert_eq!(completion, "false|true|first");
}

#[test]
fn reverse_huge_proxy_gets_lower_before_upper_has() {
    let completion = run(r#"
        const target = {
            0: "zero",
            9007199254740990: "max",
            length: 2 ** 53 + 2,
        };
        const traps = [];
        const proxy = new Proxy(target, {
            has(t, pk) {
                traps.push("Has:" + String(pk));
                if (pk === "1") {
                    throw "stop";
                }
                return Reflect.has(t, pk);
            },
            get(t, pk, r) {
                traps.push("Get:" + String(pk));
                return Reflect.get(t, pk, r);
            },
            set(t, pk, value, r) {
                traps.push("Set:" + String(pk));
                return Reflect.set(t, pk, value, r);
            },
            getOwnPropertyDescriptor(t, pk) {
                traps.push("GetOwnPropertyDescriptor:" + String(pk));
                return Reflect.getOwnPropertyDescriptor(t, pk);
            },
            defineProperty(t, pk, desc) {
                traps.push("DefineProperty:" + String(pk));
                return Reflect.defineProperty(t, pk, desc);
            },
        });
        try {
            Array.prototype.reverse.call(proxy);
        } catch (err) {
            if (err !== "stop") {
                throw err;
            }
        }
        traps.slice(0, 6).join(",");
        "#);
    assert_eq!(
        completion,
        "Get:length,Has:0,Get:0,Has:9007199254740990,Get:9007199254740990,Set:0"
    );
}

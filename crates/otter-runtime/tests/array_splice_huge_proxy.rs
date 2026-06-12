//! Runtime regression coverage for `Array.prototype.splice` with a
//! huge proxy-backed array-like receiver.
//!
//! # Contents
//! - `ArraySpeciesCreate` receives the clamped delete count.
//! - Deleted elements are copied through live proxy `Has` / `Get` /
//!   `DefineProperty` operations instead of a sparse snapshot.
//!
//! # Invariants
//! - Pathological lengths are bounded, but proxy receivers remain
//!   observable for the copied prefix and preserve abrupt completions.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.prototype.splice>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-splice-huge-proxy>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn splice_huge_proxy_copy_uses_live_traps_until_abrupt_completion() {
    let completion = run(r#"
        function StopSplice() {}
        const traps = [];
        let targetLength;
        const target = new Proxy([], {
            defineProperty(t, pk, desc) {
                traps.push("target.define:" + String(pk));
                if (pk === "0" || pk === "1") {
                    return Reflect.defineProperty(t, pk, desc);
                }
                throw new StopSplice();
            },
        });
        const array = ["no-hole", , "stop"];
        array.constructor = {
            [Symbol.species]: function(n) {
                targetLength = n;
                return target;
            },
        };
        const source = new Proxy(array, {
            get(t, pk, r) {
                traps.push("source.get:" + String(pk));
                if (pk === "length") {
                    return 2 ** 53 + 2;
                }
                return Reflect.get(t, pk, r);
            },
            has(t, pk) {
                traps.push("source.has:" + String(pk));
                return Reflect.has(t, pk);
            },
        });
        let caught = false;
        try {
            Array.prototype.splice.call(source, 0, 2 ** 53 + 4);
        } catch (err) {
            caught = err instanceof StopSplice;
        }
        targetLength + "|" + caught + "|" + traps.join(",");
        "#);
    assert_eq!(
        completion,
        "9007199254740991|true|source.get:length,source.get:constructor,source.has:0,source.get:0,target.define:0,source.has:1,source.has:2,source.get:2,target.define:2"
    );
}

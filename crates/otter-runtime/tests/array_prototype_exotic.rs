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

#[test]
fn an_inherited_index_reaches_exactly_the_arrays_whose_chain_holds_it() {
    let completion = run(r#"
        const log = [];
        // An index accessor on an unrelated object is not in any array's chain.
        Object.defineProperty({}, "0", { get() { return 1; }, set(v) { log.push("stray" + v); } });
        function fill(target, n) { for (let i = 0; i < n; i++) target[i] = i; return target; }
        for (let round = 0; round < 200; round++) fill([], 50);
        const plain = fill([], 3);
        // A subclass prototype's setter sees writes to its instances' holes.
        class Sub extends Array {}
        Object.defineProperty(Sub.prototype, "1", { set(v) { log.push("sub" + v); }, configurable: true });
        const sub = new Sub();
        sub[0] = "a";
        sub[1] = "b";
        sub.push("c");
        // `in` answers from the array's own storage only while no index can
        // be inherited.
        const holey = [, 1];
        const inBefore = [0 in plain, 3 in plain, 0 in holey, 1 in holey, 1 in sub, 6 in holey];
        // A new [[Prototype]] for %Array.prototype% reaches every plain array,
        // including writes from code that already ran hot.
        Object.setPrototypeOf(Array.prototype, {
            set 5(v) { log.push("proto" + v); },
        });
        const late = fill([], 7);
        const inAfter = 5 in holey;
        [
            plain.join(),
            sub.length, sub.hasOwnProperty(1), sub[0],
            late.length, late.hasOwnProperty(5),
            inBefore.join(), inAfter,
            log.join(),
        ].join("|");
        "#);
    assert_eq!(
        completion,
        "0,1,2|2|false|a|7|false|true,false,false,true,true,false|true|subb,subc,proto5"
    );
}

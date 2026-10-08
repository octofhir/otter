//! The built-in iteration protocol stays observable.
//!
//! GetIterator reads `@@iterator` and `next`, IteratorClose reads `return`.
//! Built-in arrays, Maps, Sets, strings and iterators skip those reads only
//! while their realm prototypes are unmodified; each test replaces one of
//! them and checks every iteration form observes the replacement, then
//! restores it and checks iteration is built-in again.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<iteration-protocol-test>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn replaced_array_iterator_next_is_called() {
    let out = run(r#"
        var log = [];
        var AIP = Object.getPrototypeOf([][Symbol.iterator]());
        var next = AIP.next;
        AIP.next = function () { log.push("next"); return next.call(this); };
        var [a, b] = [1, 2];
        for (var x of [3]) log.push(x);
        log.push([...[4]].length);
        AIP.next = next;
        var [c] = [5];
        log.push(a, b, c);
        log.join(",");
    "#);
    assert_eq!(out, "next,next,next,3,next,next,next,1,1,2,5");
}

#[test]
fn replaced_iterator_return_closes() {
    let out = run(r#"
        var log = [];
        var AIP = Object.getPrototypeOf([][Symbol.iterator]());
        AIP.return = function () { log.push("return"); return {}; };
        var [a] = [1, 2];
        for (var x of [3, 4]) break;
        delete AIP.return;
        var [b] = [5, 6];
        log.push(a, b);
        log.join(",");
    "#);
    assert_eq!(out, "return,return,1,5");
}

#[test]
fn own_next_on_builtin_iterator_is_called() {
    let out = run(r#"
        var log = [];
        var it = [1, 2][Symbol.iterator]();
        it.next = function () { log.push("own"); return { done: true }; };
        for (var x of { [Symbol.iterator]: function () { return it; } }) log.push(x);
        log.join(",");
    "#);
    assert_eq!(out, "own");
}

#[test]
fn replaced_collection_iterators_are_called() {
    let out = run(r#"
        var log = [];
        var MIP = Object.getPrototypeOf(new Map()[Symbol.iterator]());
        var mapNext = MIP.next;
        MIP.next = function () { log.push("map"); return mapNext.call(this); };
        for (var [k] of new Map([[1, 0]])) log.push(k);
        MIP.next = mapNext;
        Set.prototype[Symbol.iterator] = function* () { yield "set"; };
        for (var v of new Set([2])) log.push(v);
        String.prototype[Symbol.iterator] = function* () { yield "str"; };
        log.push(...("ab"));
        log.join(",");
    "#);
    assert_eq!(out, "map,1,map,set,str");
}

#[test]
fn removed_array_iterator_makes_arrays_not_iterable() {
    let out = run(r#"
        var log = [];
        var values = Array.prototype[Symbol.iterator];
        delete Array.prototype[Symbol.iterator];
        try { var [a] = [1]; } catch (e) { log.push(e instanceof TypeError); }
        Array.prototype[Symbol.iterator] = values;
        var [b] = [2];
        log.push(b);
        log.join(",");
    "#);
    assert_eq!(out, "true,2");
}

#[test]
fn replaced_generator_protocol_is_called() {
    let out = run(r#"
        var log = [];
        function* g() { try { yield 1; yield 2; yield 3; } finally { log.push("finally"); } }
        var GP = Object.getPrototypeOf(g.prototype);
        var next = GP.next, ret = GP.return;
        for (var x of g()) { log.push(x); if (x === 2) break; }
        GP.next = function () { log.push("n"); return next.call(this); };
        for (var y of g()) log.push(y);
        GP.next = next;
        GP.return = function () { log.push("r"); return ret.call(this); };
        for (var z of g()) break;
        GP.return = ret;
        var own = g();
        own.next = function () { log.push("own"); return { done: true }; };
        for (var w of own) log.push(w);
        g.prototype.next = function () { log.push("fnproto"); return { done: true }; };
        for (var v of g()) log.push(v);
        delete g.prototype.next;
        log.push([...g()].length, new Set(g()).size);
        log.join(",");
    "#);
    assert_eq!(
        out,
        "1,2,finally,n,1,n,2,n,3,n,finally,r,finally,own,fnproto,finally,finally,3,3"
    );
}

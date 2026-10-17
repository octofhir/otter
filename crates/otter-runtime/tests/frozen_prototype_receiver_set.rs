//! A frozen prototype does not stop its instances from getting properties.
//!
//! [[Set]] asks only the receiver whether it is extensible (§10.1.9.2). A
//! consent manager (Transcend's airgap, on Notion) freezes the prototype of
//! its `class extends Array` and pushes onto an instance; the push threw
//! "Cannot assign to read only property '0'".

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/run.js",
        )
        .expect("script runs")
        .completion_string()
        .to_owned()
}

#[test]
fn an_array_under_a_frozen_prototype_takes_elements() {
    let result = run(r#"
        'use strict';
        class List extends Array { constructor() { super(); } }
        Object.freeze(List.prototype);
        Object.freeze(List);
        const list = new List();
        list.push('a');
        Reflect.apply(Array.prototype.push, list, ['b']);
        const plain = [];
        Object.setPrototypeOf(plain, Object.freeze(Object.create(Array.prototype)));
        plain[0] = 'c';
        plain.named = 'd';
        [list.length, list.join(''), plain.length, plain[0], plain.named].join()
    "#);
    assert_eq!(result, "2,ab,1,c,d");
}

#[test]
fn a_frozen_array_still_refuses() {
    let result = run(r#"
        'use strict';
        const out = [];
        for (const f of [() => Object.freeze([]).push(1), () => { const a = Object.freeze([]); a[0] = 1; }]) {
            try { f(); out.push('no error'); } catch (error) { out.push(error instanceof TypeError); }
        }
        const sealedProtoChild = Object.create(Object.freeze({ fixed: 1 }));
        try { sealedProtoChild.fixed = 2; out.push('no error'); } catch (error) { out.push(error instanceof TypeError); }
        out.join()
    "#);
    assert_eq!(result, "true,true,true");
}

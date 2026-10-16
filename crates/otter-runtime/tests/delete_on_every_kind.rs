//! `delete` works on an object of any kind, by name and by computed key.
//!
//! Objects the engine keeps in a heap kind of their own — a Map, a Promise,
//! an ArrayBuffer, a generator, an Intl formatter — are ordinary as far as
//! `delete` is concerned (§10.1.10): a key they do not have deletes
//! vacuously, and one set on them goes. A computed `delete o[key]` on them
//! threw an internal "type mismatch" instead, which broke lodash's
//! `getRawTag` (`delete value[Symbol.toStringTag]`) on every such value,
//! and `in` threw the same on a generator. A property set on a promise or a
//! buffer was readable but not own as far as `in`, `hasOwnProperty` and
//! `Object.keys` could tell.

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
fn delete_by_any_key_on_any_kind_of_object() {
    let result = run(r#"
        const tag = Symbol.toStringTag;
        const values = {
            map: new Map(), set: new Set(), weakMap: new WeakMap(), weakSet: new WeakSet(),
            promise: Promise.resolve(), buffer: new ArrayBuffer(1), view: new DataView(new ArrayBuffer(1)),
            bytes: new Uint8Array(1), regexp: /a/, generator: (function* () {})(),
            format: new Intl.NumberFormat(), date: new Date(), error: new Error('x'),
        };
        const out = [];
        for (const [name, value] of Object.entries(values)) {
            const key = 'extra';
            const results = [delete value[tag], delete value.missing, delete value['missing']];
            value[key] = 1;
            value[tag] = 'Tagged';
            const sym = Symbol('own');
            value[sym] = 2;
            results.push(key in value, sym in value, Object.keys(value).includes(key));
            results.push(delete value[key], key in value, delete value[sym], sym in value);
            out.push(name + ':' + results.join(','));
        }
        out.join(' ')
    "#);
    for entry in result.split(' ') {
        assert!(
            entry.ends_with(":true,true,true,true,true,true,true,false,true,false"),
            "{entry} in {result}"
        );
    }
}

#[test]
fn delete_on_undefined_or_null_throws_a_type_error() {
    let result = run(r#"
        const out = [];
        for (const value of [undefined, null]) {
            try { delete value[Symbol.iterator]; out.push('no throw'); }
            catch (error) { out.push(error.constructor.name); }
            try { delete value.name; out.push('no throw'); }
            catch (error) { out.push(error.constructor.name); }
        }
        out.push(delete (5)[Symbol.iterator], delete (true).x, delete 'ab'[5]);
        out.join(',')
    "#);
    assert_eq!(
        result,
        "TypeError,TypeError,TypeError,TypeError,true,true,true"
    );
}

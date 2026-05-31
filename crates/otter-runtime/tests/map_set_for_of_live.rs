//! Runtime coverage for live `for…of` iteration over `Map` and `Set`
//! (§24.1.5.1 / §24.2.5.1 CreateMapIterator / CreateSetIterator) — the
//! iterator walks the backing table by index, so additions and
//! deletions during iteration are observed.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-createmapiterator>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<map-set-forof-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn map_for_of_observes_deletion() {
    // Deleting an as-yet-unvisited entry mid-iteration skips it.
    let out = run(r#"
        var m = new Map(); m.set(0, 'a'); m.set(1, 'b');
        var n = 0;
        for (var x of m) { m.delete(1); n += 1; }
        String(n);
    "#);
    assert_eq!(out, "1");
}

#[test]
fn map_for_of_observes_addition() {
    let out = run(r#"
        var m = new Map(); m.set(0, 'a');
        var n = 0;
        for (var x of m) { if (n === 0) m.set(1, 'b'); n += 1; if (n > 5) break; }
        String(n);
    "#);
    assert_eq!(out, "2");
}

#[test]
fn map_for_of_yields_key_value_pairs() {
    let out = run(r#"
        var m = new Map([['k', 'v']]);
        var s = '';
        for (var e of m) { s += e[0] + '=' + e[1]; }
        s;
    "#);
    assert_eq!(out, "k=v");
}

#[test]
fn set_for_of_observes_addition() {
    let out = run(r#"
        var s = new Set([1]);
        var n = 0;
        for (var x of s) { if (n === 0) s.add(2); n += 1; if (n > 5) break; }
        String(n);
    "#);
    assert_eq!(out, "2");
}

#[test]
fn set_for_of_yields_values() {
    assert_eq!(
        run("var t = 0; for (var x of new Set([1, 2, 3])) t += x; String(t);"),
        "6"
    );
}

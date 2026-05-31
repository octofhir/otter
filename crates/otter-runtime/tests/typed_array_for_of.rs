//! Runtime regression coverage for `for…of` over TypedArrays and the
//! live `%TypedArray%.prototype.{values,keys,entries}` iterators
//! (§23.2.3.32 / §23.2.5.1 CreateArrayIterator).
//!
//! # Contents
//! - Plain `for…of` sums TypedArray elements.
//! - Element mutation mid-iteration is observed (the iterator is live,
//!   not a snapshot).
//! - `keys()` / `entries()` yield indices / `[index, value]` pairs.
//!
//! # Invariants
//! - The TypedArray iterator reads `ta[index]` on every step, so
//!   writes through the same view during iteration are visible.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-createarrayiterator>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<ta-forof-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn for_of_sums_typed_array() {
    assert_eq!(
        run("var s = 0; for (var x of new Int8Array([1, 2, 3])) s += x; String(s);"),
        "6"
    );
}

#[test]
fn for_of_observes_live_mutation() {
    // array[1] is overwritten on the first iteration; the second step
    // must read the new value (64), proving the iterator is live.
    let out = run(r#"
        var a = new Int8Array([3, 2, 4, 1]);
        var out = [];
        for (var x of a) { out.push(x); a[1] = 64; }
        out.join(",");
    "#);
    assert_eq!(out, "3,64,4,1");
}

#[test]
fn typed_array_keys_yields_indices() {
    let out = run(r#"
        var k = [];
        for (var i of new Uint16Array([9, 8, 7]).keys()) k.push(i);
        k.join(",");
    "#);
    assert_eq!(out, "0,1,2");
}

#[test]
fn typed_array_entries_yields_pairs() {
    let out = run(r#"
        var e = [];
        for (var p of new Float64Array([9, 8]).entries()) e.push(p.join(":"));
        e.join(",");
    "#);
    assert_eq!(out, "0:9,1:8");
}

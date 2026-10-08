//! Spread elements run IteratorToList.
//!
//! `[...x]` and `f(...x)` append through one `SpreadAppend`: proven
//! built-in iterables copy directly, every other iterable runs the
//! observable protocol, a throwing step propagates without closing the
//! iterator, holes read through the prototype, and a replaced iterator
//! `next` is called. Each case runs hot enough to reach compiled code.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn spread_elements_run_iterator_to_list() {
    let source = r#"// Spread runs IteratorToList: observable protocol unless proven, never closes.
const out = [];
const log = [];
function* gen(n) { for (let i = 0; i < n; i++) yield { i }; }
const user = {
  [Symbol.iterator]() {
    let i = 0;
    return {
      next() { log.push("n" + i); return i < 3 ? { value: { v: i++ }, done: false } : { done: true }; },
      return() { log.push("ret"); return {}; },
    };
  },
};
const throwing = {
  [Symbol.iterator]() {
    return { next() { throw new Error("boom"); }, return() { log.push("closed"); return {}; } };
  },
};
function sum(...xs) { let s = 0; for (const x of xs) s += typeof x === "object" ? (x.i ?? x.v ?? 0) : x; return s; }
for (let round = 0; round < 2000; round++) {
  out.length = 0; log.length = 0;
  out.push([...gen(4)].length, sum(...gen(4)));
  out.push(JSON.stringify([...user].map((o) => o.v)));
  try { [...throwing]; } catch (e) { out.push(e.message); }
  out.push(JSON.stringify([...[1, , 3]]));
  out.push(JSON.stringify([...new Map([[1, { a: 1 }], [2, 2]])]));
  out.push([...new Set(["a", "b"])].join(""), [..."a\u{1F600}b"].length);
  out.push(sum(1, ...[2, 3], 4, ...new Set([5])));
  out.push((function () { return [...arguments].join(""); })(7, 8, 9));
  out.push([...new Uint8Array([4, 5])].join(""), [...[1, 2].entries()].join("|"));
  out.push([...[1, 2, 3].values().map((x) => x * 2)].join(""));
  out.push(Math.max(...Array.from({ length: 300 }, (_, i) => i)));
  out.push(log.join(""));
}
Array.prototype[1] = "proto";
out.push(JSON.stringify([...[0, , 2]]));
delete Array.prototype[1];
const AIP = Object.getPrototypeOf([][Symbol.iterator]());
const next = AIP.next;
let calls = 0;
AIP.next = function () { calls++; return next.call(this); };
out.push([...[1, 2]].length, calls);
AIP.next = next;
out.join(" ; ");
"#;
    let mut rt = Runtime::builder().build().expect("runtime");
    let out = rt
        .run_script(SourceInput::from_javascript(source), "<spread-test>")
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(
        out,
        r#"4 ; 6 ; [0,1,2] ; boom ; [1,null,3] ; [[1,{"a":1}],[2,2]] ; ab ; 3 ; 15 ; 789 ; 45 ; 0,1|1,2 ; 246 ; 299 ; n0n1n2n3 ; [0,"proto",2] ; 2 ; 3"#
    );
}

// Hot `== null`, `!= null`, `== undefined` and `!= undefined` over every
// kind of value — immediates, numbers, strings, symbols, BigInts, objects,
// closures and native functions — both branched on and kept as values,
// with the constant on either side.
const values = [
  null, undefined, 0, -0, 1, 1.5, NaN, Infinity, "", "x", true, false,
  Symbol("s"), 10n, 0n, {}, [], [null], function () {}, () => null,
  Math.max, Object, Array.prototype.push, new Map(), /r/, new Date(0),
];
function classify(n) {
  let eqNull = 0, neNull = 0, eqUndef = 0, neUndef = 0, kept = 0;
  for (let i = 0; i < n; i++) {
    const v = values[i % values.length];
    if (v == null) eqNull++;
    if (v != null) neNull++;
    if (undefined == v) eqUndef++;
    if (undefined != v) neUndef++;
    const a = v == null, b = null != v, c = v == undefined;
    kept += (a ? 1 : 0) + (b ? 2 : 0) + (c ? 4 : 0);
  }
  return [eqNull, neNull, eqUndef, neUndef, kept].join(",");
}
function walk(n) {
  // A linked list ending in null, then in undefined.
  let head = null;
  for (let i = 0; i < 50; i++) head = { next: head, v: i };
  let total = 0;
  for (let r = 0; r < n; r++) {
    for (let p = head; p != null; p = p.next) total += p.v;
    let q = head;
    while (!(q == undefined)) { total -= q.v; q = q.next || undefined; }
  }
  return total;
}
console.log(classify(60000));
console.log(walk(2000));

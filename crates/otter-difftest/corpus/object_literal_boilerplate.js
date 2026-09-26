// Static-key object literals built in one step (`NewObjectLiteral`) next to
// every literal form that keeps the per-property path: property order,
// function-name inference, attributes, `__proto__`, index-like and duplicate
// keys, accessors, spreads, computed keys and `super` methods. Hot loops tier
// the sites up; values are evaluated in source order before the object exists.
function plain(a, b) { return { x: a, y: b, label: "p" + a }; }
function wide(a) {
  return { k0: a, k1: a + 1, k2: a + 2, k3: a + 3, k4: a + 4, k5: [a], k6: { n: a } };
}
function named() { return { f: function () {}, g: () => 1, h() { return 2; } }; }
function order(log) {
  return { first: log.push("a"), second: log.push("b"), third: log.push("c") };
}
function proto(p) { return { __proto__: p, own: 1 }; }
function indexed(v) { return { 0: v, "1": v + 1, two: v + 2 }; }
function duplicate(v) { return { a: v, b: 2, a: v + 10 }; }
function accessor(v) { return { get x() { return v; }, y: v }; }
function spread(src) { return { ...src, extra: 1 }; }
function computed(k, v) { return { [k]: v, fixed: v }; }
const base = { hello() { return "base"; } };
function withSuper() { return { __proto__: base, hello() { return super.hello() + "!"; } }; }

function describe(o) {
  const keys = Object.keys(o);
  const attrs = keys.map((k) => {
    const d = Object.getOwnPropertyDescriptor(o, k);
    return k + ":" + (d.writable ? "w" : "") + (d.enumerable ? "e" : "") + (d.configurable ? "c" : "");
  });
  return keys.join(",") + "|" + attrs.join(",");
}

let checksum = 0;
for (let round = 0; round < 3000; round++) {
  const p = plain(round, round * 2);
  checksum = (checksum + p.x + p.y + p.label.length) | 0;
  const w = wide(round);
  checksum = (checksum + w.k4 + w.k5[0] + w.k6.n) | 0;
}

const log = [];
const results = [
  checksum,
  describe(plain(1, 2)),
  describe(wide(3)),
  JSON.stringify(wide(3)),
  (() => { const n = named(); return [n.f.name, n.g.name, n.h.name, n.h()].join(); })(),
  JSON.stringify(order(log)),
  log.join(),
  Object.getPrototypeOf(proto(base)) === base,
  JSON.stringify(proto(base)),
  describe(indexed(5)),
  JSON.stringify(indexed(5)),
  JSON.stringify(duplicate(1)),
  describe(duplicate(1)),
  (() => { const a = accessor(4); return typeof Object.getOwnPropertyDescriptor(a, "x").get + a.x + a.y; })(),
  JSON.stringify(spread({ s: 1, t: 2 })),
  JSON.stringify(computed("dyn", 7)),
  withSuper().hello(),
  Object.getPrototypeOf(plain(1, 2)) === Object.prototype,
  plain(1, 2) !== plain(1, 2),
];
console.log(JSON.stringify(results));

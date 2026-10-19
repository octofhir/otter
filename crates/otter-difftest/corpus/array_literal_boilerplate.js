// Constant array literals share one copy-on-write element store per literal.
// Every mutation path copies out first, so sibling arrays of the same literal
// never observe another's write, in every tier.
function make() { return [1, 2, 3, 4]; }
function mixed() { return ["a", "b", null, true, , 5.5, -0, void 0]; }
const out = [];
function check(label, mutate, factory = make) {
  const a = factory(), b = factory();
  const r = mutate(a);
  out.push(label + ":" + JSON.stringify(a) + "|" + JSON.stringify(b) + "|" + JSON.stringify(r)
    + "|" + Object.is(b[6], -0) + "|" + (4 in b));
}
function round(log) {
  const before = out.length;
  check("store", (a) => { a[0] = 9; });
  check("storeObj", (a) => { a[1] = { x: 1 }; });
  check("push", (a) => a.push(5));
  check("pop", (a) => a.pop());
  check("shift", (a) => a.shift());
  check("unshift", (a) => a.unshift(0));
  check("splice", (a) => a.splice(1, 2, "z"));
  check("sort", (a) => a.sort((x, y) => y - x));
  check("reverse", (a) => a.reverse());
  check("fill", (a) => a.fill(7, 1));
  check("copyWithin", (a) => a.copyWithin(0, 2));
  check("shrink", (a) => { a.length = 1; });
  check("grow", (a) => { a.length = 6; });
  check("delete", (a) => delete a[2]);
  check("accessor", (a) => Object.defineProperty(a, 0, { get() { return 42; } }));
  check("freeze", (a) => { Object.freeze(a); try { a[0] = 5; } catch (e) {} return Object.isFrozen(a); });
  check("hole", (a) => { a[1] = undefined; a.push(1); }, mixed);
  check("fillHole", (a) => { a[4] = "filled"; }, mixed);
  check("widen", (a) => { a[0] = "s"; });
  check("keyed", (a) => { for (let i = 0; i < a.length; i++) a[i] = a[i] * 2; return a.length; });
  check("spread", (a) => [...a, ...a].length);
  if (!log) out.length = before;
}
for (let i = 0; i < 3000; i++) round(i === 0 || i === 2999);
console.log(out.join("\n"));

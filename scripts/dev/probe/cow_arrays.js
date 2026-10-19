// Copy-on-write literal storage: every mutation path must copy out first and
// leave sibling arrays of the same literal untouched, in every tier.
function make() { return [1, 2, 3, 4]; }
function strs() { return ["a", "b", null, true, , 5.5]; }
const out = [];
function check(label, mutate, factory = make) {
  const a = factory(), b = factory();
  const r = mutate(a);
  out.push(label + ":" + JSON.stringify(a) + "|" + JSON.stringify(b) + "|" + JSON.stringify(r));
}
for (let round = 0; round < 3000; round++) {
  const log = round === 0 || round === 2999;
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
  check("len0", (a) => { a.length = 1; });
  check("lenGrow", (a) => { a.length = 6; });
  check("delete", (a) => delete a[2]);
  check("define", (a) => Object.defineProperty(a, 0, { get() { return 42; } }));
  check("freeze", (a) => { Object.freeze(a); try { a[0] = 5; } catch (e) {} return Object.isFrozen(a); });
  check("hole", (a) => { a[1] = undefined; a.push(1); }, strs);
  check("strStore", (a) => { a[4] = "filled"; }, strs);
  check("widen", (a) => { a[0] = "s"; });
  if (!log) out.length = before;
}
console.log(out.join("\n"));
